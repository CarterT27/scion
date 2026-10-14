//! SPEC 19.0 Corpus: edge-case catalog sanity. Every repository is a valid history under the
//! SPEC 4.4 path rules and realizes the edge case its name promises.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test helpers

use scion_refmodel::MTree;
use scion_testkit::{GenRepo, edge_cases};
use std::collections::{BTreeMap, BTreeSet};

fn valid_name(n: &[u8]) -> bool {
    !n.is_empty()
        && n.len() <= 255
        && !n.contains(&0)
        && !n.contains(&b'/')
        && n != b"."
        && n != b".."
        && !n.eq_ignore_ascii_case(b".scion")
}

fn check_tree(name: &str, t: &MTree) {
    for (p, f) in t {
        assert!(p.len() <= 4096, "{name}: path of {} bytes", p.len());
        assert!(
            p.split(|b| *b == b'/').all(valid_name),
            "{name}: invalid path {:?}",
            String::from_utf8_lossy(p)
        );
        assert!((1..=3).contains(&f.mode), "{name}: bad mode");
        let mut prefix = p.clone();
        prefix.push(b'/');
        assert!(
            !t.range(prefix.clone()..)
                .next()
                .is_some_and(|(q, _)| q.starts_with(&prefix)),
            "{name}: file/dir conflict"
        );
    }
}

fn catalog() -> BTreeMap<&'static str, GenRepo> {
    let list = edge_cases();
    let map: BTreeMap<&'static str, GenRepo> = list.iter().cloned().collect();
    assert_eq!(map.len(), list.len(), "names are unique");
    map
}

fn all_trees(r: &GenRepo) -> impl Iterator<Item = &MTree> {
    r.commits.iter().map(|c| &c.tree)
}

#[test]
fn catalog_is_complete_and_valid() {
    let cat = catalog();
    for name in [
        "empty_root",
        "deep_path_4096",
        "deep_nesting_4096",
        "long_names_255",
        "case_only_names",
        "file_dir_flip",
        "crlf",
        "nul_content",
        "exec_flip",
        "symlink_retarget",
        "delete_and_readd",
        "unterminated_last_line",
        "empty_file",
        "merge_first_parent",
    ] {
        assert!(cat.contains_key(name), "missing {name}");
    }
    for (name, r) in &cat {
        assert!(r.commits.len() >= 2, "{name}: at least two commits");
        assert_eq!(r.roots(), vec![0], "{name}: single root");
        let mut ids = BTreeSet::new();
        for (i, c) in r.commits.iter().enumerate() {
            assert!(c.parents.iter().all(|p| *p < i), "{name}: topological");
            if i > 0 {
                assert_ne!(
                    c.tree, r.commits[c.parents[0]].tree,
                    "{name}: commit {i} empty"
                );
            }
            ids.insert(c.change_id);
            check_tree(name, &c.tree);
        }
        assert_eq!(ids.len(), r.commits.len(), "{name}: distinct change ids");
    }
}

#[test]
fn catalog_is_deterministic() {
    assert_eq!(format!("{:?}", edge_cases()), format!("{:?}", edge_cases()));
}

#[test]
fn each_case_realizes_its_edge() {
    let cat = catalog();

    assert!(all_trees(&cat["empty_root"]).any(|t| t.is_empty()));
    assert!(cat["empty_root"].commits[0].tree.is_empty());

    let deep = &cat["deep_path_4096"];
    assert!(all_trees(deep).any(|t| t.keys().any(|p| p.len() == 4096)));

    let nested = &cat["deep_nesting_4096"];
    assert!(all_trees(nested).any(|t| {
        t.keys()
            .any(|p| p.len() == 4096 && p.split(|b| *b == b'/').count() == 2048)
    }));

    let long = &cat["long_names_255"];
    let comps: Vec<Vec<u8>> = long.commits[0]
        .tree
        .keys()
        .flat_map(|p| p.split(|b| *b == b'/').map(|c| c.to_vec()))
        .collect();
    assert!(comps.iter().all(|c| c.len() == 255));
    assert!(
        comps.iter().any(|c| std::str::from_utf8(c).is_err()),
        "non-UTF-8 name"
    );
    assert!(
        comps
            .iter()
            .any(|c| std::str::from_utf8(c).is_ok_and(|s| !s.is_ascii()))
    );
    assert!(
        long.commits[0].tree.keys().any(|p| p.contains(&b'/')),
        "255-byte directory"
    );

    let case = &cat["case_only_names"];
    for t in all_trees(case) {
        let lower: BTreeSet<Vec<u8>> = t.keys().map(|p| p.to_ascii_lowercase()).collect();
        assert!(lower.len() < t.len(), "case-only collisions");
    }

    let flip = &cat["file_dir_flip"];
    let mut file_to_dir = false;
    let mut dir_to_file = false;
    for c in &flip.commits[1..] {
        let prev = &flip.commits[c.parents[0]].tree;
        for p in prev.keys() {
            let mut pre = p.clone();
            pre.push(b'/');
            if c.tree.keys().any(|q| q.starts_with(&pre)) {
                file_to_dir = true;
            }
        }
        for p in c.tree.keys() {
            let mut pre = p.clone();
            pre.push(b'/');
            if prev.keys().any(|q| q.starts_with(&pre)) {
                dir_to_file = true;
            }
        }
    }
    assert!(file_to_dir && dir_to_file);

    let crlf = &cat["crlf"];
    assert!(all_trees(crlf).any(|t| {
        t.values()
            .any(|f| f.content.windows(2).any(|w| w == b"\r\n"))
    }));

    let nul = &cat["nul_content"];
    assert!(all_trees(nul).any(|t| t.values().any(|f| f.content.contains(&0))));

    let exec = &cat["exec_flip"];
    let modes: Vec<u8> = exec
        .commits
        .iter()
        .map(|c| c.tree[b"run.sh".as_slice()].mode)
        .collect();
    assert_eq!(modes, vec![1, 2, 1]);
    let contents: BTreeSet<&Vec<u8>> = exec
        .commits
        .iter()
        .map(|c| &c.tree[b"run.sh".as_slice()].content)
        .collect();
    assert_eq!(contents.len(), 1, "same content, only the mode flips");

    let link = &cat["symlink_retarget"];
    let targets: Vec<(u8, Vec<u8>)> = link
        .commits
        .iter()
        .map(|c| {
            let f = &c.tree[b"link".as_slice()];
            (f.mode, f.content.clone())
        })
        .collect();
    assert_eq!(targets[0], (3, b"a.txt".to_vec()));
    assert_eq!(targets[1], (3, b"b.txt".to_vec()));
    assert_eq!(
        targets[2],
        (1, b"b.txt".to_vec()),
        "symlink to file with same bytes"
    );

    let readd = &cat["delete_and_readd"];
    let x: Vec<Option<&Vec<u8>>> = readd
        .commits
        .iter()
        .map(|c| c.tree.get(b"x.txt".as_slice()).map(|f| &f.content))
        .collect();
    assert!(x[0].is_some() && x[1].is_none() && x[2] == x[0]);
    assert!(x[3].is_none() && x[4].is_some() && x[4] != x[0]);

    let unterminated = &cat["unterminated_last_line"];
    assert!(all_trees(unterminated).any(|t| t.values().any(|f| !f.content.ends_with(b"\n"))));
    assert!(all_trees(unterminated).any(|t| t.values().any(|f| f.content.ends_with(b"\n"))));

    let empty = &cat["empty_file"];
    assert!(all_trees(empty).any(|t| t.values().any(|f| f.content.is_empty())));

    let mfp = &cat["merge_first_parent"];
    let merge = mfp
        .commits
        .last()
        .cloned()
        .unwrap_or_else(|| unreachable!());
    assert_eq!(merge.parents, vec![1, 2]);
    assert_eq!(
        merge.tree[b"m.txt".as_slice()],
        mfp.commits[1].tree[b"m.txt".as_slice()],
        "keeps the first parent's version"
    );
    assert!(
        merge.tree.contains_key(b"t.txt".as_slice()),
        "takes theirs-only additions"
    );
}
