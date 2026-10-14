//! Property tests for SPEC 6.2 path evaluation and the PLAN 6.2 epoch (P05): evaluation agrees
//! with a naive longest-prefix model, and the epoch is independent of rule/member order and of
//! login case.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test helpers

use proptest::prelude::*;
use scion_core::Principal;
use scion_policy::Policy;
use std::collections::BTreeMap;

const NAMES: [&str; 3] = ["a", "b", "ab"];
const USERS: [&str; 4] = ["u0", "u1", "u2", "u3"];

type Grant = Option<Vec<&'static str>>;
type Rules = BTreeMap<Vec<&'static str>, (Grant, Grant)>;

fn gh(login: &str) -> Principal {
    Principal::GitHub {
        login: login.into(),
        id: None,
    }
}

/// A path of 0..=3 components over a tiny alphabet, so prefixes collide often.
fn path() -> impl Strategy<Value = Vec<&'static str>> {
    prop::collection::vec(prop::sample::select(&NAMES[..]), 0..=3)
}

fn grant() -> impl Strategy<Value = Grant> {
    prop::option::of(prop::collection::vec(
        prop::sample::select(&USERS[..]),
        0..=3,
    ))
}

fn rules() -> impl Strategy<Value = Rules> {
    (
        prop::collection::btree_map(path(), (grant(), grant()), 0..6),
        grant(),
        grant(),
    )
        .prop_map(|(mut m, r, w)| {
            m.insert(Vec::new(), (r, w));
            m
        })
}

fn render(rules: &[(&Vec<&str>, &(Grant, Grant))], upper: bool) -> String {
    let list = |l: &Vec<&str>| {
        let items: Vec<String> = l
            .iter()
            .map(|u| {
                let u = if upper {
                    u.to_uppercase()
                } else {
                    (*u).to_owned()
                };
                format!("\"github:{u}\"")
            })
            .collect();
        format!("[{}]", items.join(", "))
    };
    let mut s = String::from("version = 1\n");
    for (p, (r, w)) in rules {
        s.push_str("[[paths]]\n");
        s.push_str(&format!("prefix = \"/{}\"\n", p.join("/")));
        if let Some(r) = r {
            s.push_str(&format!("read = {}\n", list(r)));
        }
        if let Some(w) = w {
            s.push_str(&format!("write = {}\n", list(w)));
        }
    }
    s
}

/// Naive model: scan every rule and keep the longest component-wise prefix that names the
/// permission; the root rule always governs as a fallback (missing key = nobody).
fn model(rules: &Rules, path: &[&str], user: &str, write: bool) -> bool {
    let mut best: Option<(usize, Vec<&str>)> = None;
    for (p, (r, w)) in rules {
        let g = if write { w } else { r };
        let g = match g {
            Some(g) => g.clone(),
            None if p.is_empty() => Vec::new(),
            None => continue,
        };
        let matches = p.len() <= path.len() && p[..] == path[..p.len()];
        if matches && best.as_ref().is_none_or(|(n, _)| p.len() >= *n) {
            best = Some((p.len(), g));
        }
    }
    best.is_some_and(|(_, g)| g.contains(&user))
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(scion_core::test_support::proptest_cases(256)))]

    #[test]
    fn evaluation_matches_model_and_epoch_is_order_independent(
        rules in rules(),
        order in any::<prop::sample::Index>(),
        probes in prop::collection::vec((path(), prop::sample::select(&USERS[..])), 1..16),
    ) {
        let entries: Vec<_> = rules.iter().collect();
        let mut shuffled = entries.clone();
        let k = order.index(shuffled.len());
        shuffled.rotate_left(k);
        shuffled.reverse();
        let p1 = Policy::parse(&render(&entries, false)).unwrap();
        let p2 = Policy::parse(&render(&shuffled, true)).unwrap();
        prop_assert_eq!(p1.epoch(), p2.epoch());
        for (path, user) in probes {
            let bytes = path.join("/");
            let who = gh(user);
            let r = model(&rules, &path, user, false);
            let w = model(&rules, &path, user, true);
            prop_assert_eq!(p1.can_read_bytes(&who, bytes.as_bytes()), r);
            prop_assert_eq!(p1.can_write_bytes(&who, bytes.as_bytes()), w);
            prop_assert_eq!(p2.can_read_bytes(&who, bytes.as_bytes()), r);
            prop_assert_eq!(p2.can_write_bytes(&who, bytes.as_bytes()), w);
        }
    }
}
