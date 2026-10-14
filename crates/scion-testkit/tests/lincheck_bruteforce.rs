//! SPEC 19.5 linearizability checker cross-validated against a brute-force oracle on small
//! random histories (both linearizable and mutated ones).
//!
//! The oracle is written independently of the checker: it enumerates the initial state, the
//! subset of Unknown operations that took effect, the targets of unknown lands, and every
//! real-time-respecting order of the operations.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test helpers

use rand::{Rng, SeedableRng};
use rand_chacha::ChaCha8Rng;
use scion_testkit::lincheck::{Event, RefOp, RefRet, check_ref_registers};

const FRESH: u8 = 255;

fn t(n: u8) -> [u8; 34] {
    let mut x = [0u8; 34];
    x[0] = n;
    x
}

#[derive(Clone, Debug)]
struct Op {
    call: RefOp,
    /// `None` = Unknown.
    ret: Option<RefRet>,
    inv: u64,
    resp: u64,
}

fn tgt(x: &[u8; 34]) -> u8 {
    x[0]
}

/// Brute force: is there a legal sequential order?
fn oracle(ops: &[Op]) -> bool {
    // Fail ops and unknown reads have no effect and observe nothing.
    let ops: Vec<&Op> = ops
        .iter()
        .filter(|o| !matches!(o.ret, Some(RefRet::Fail)))
        .filter(|o| !(o.ret.is_none() && matches!(o.call, RefOp::Read)))
        .collect();
    let unknown: Vec<usize> = (0..ops.len()).filter(|i| ops[*i].ret.is_none()).collect();
    let mut targets: Vec<u8> = vec![FRESH];
    for o in &ops {
        match (&o.call, &o.ret) {
            (_, Some(RefRet::Value { target, .. })) | (_, Some(RefRet::Ok { target, .. })) => {
                targets.push(tgt(target))
            }
            (RefOp::Cas { target, .. }, _) => targets.push(tgt(target)),
            _ => {}
        }
    }
    targets.sort();
    targets.dedup();
    for mask in 0u32..(1 << unknown.len()) {
        let included: Vec<usize> = (0..ops.len())
            .filter(|i| match unknown.iter().position(|u| u == i) {
                Some(k) => mask & (1 << k) != 0,
                None => true,
            })
            .collect();
        for v0 in 0..=9u64 {
            for &t0 in &targets {
                let mut done = vec![false; ops.len()];
                if dfs(&ops, &included, &mut done, (v0, t0), &targets) {
                    return true;
                }
            }
        }
    }
    false
}

fn dfs(ops: &[&Op], inc: &[usize], done: &mut Vec<bool>, st: (u64, u8), targets: &[u8]) -> bool {
    if inc.iter().all(|i| done[*i]) {
        return true;
    }
    for &i in inc {
        if done[i] {
            continue;
        }
        // Every included op that returned before i was invoked must already be done. An
        // Unknown op never "returned": it may take effect at any time after its invocation.
        let blocked = inc
            .iter()
            .any(|&j| j != i && !done[j] && ops[j].ret.is_some() && ops[j].resp < ops[i].inv);
        if blocked {
            continue;
        }
        let o = ops[i];
        let mut nexts: Vec<(u64, u8)> = Vec::new();
        match (&o.call, &o.ret) {
            (RefOp::Read, Some(RefRet::Value { version, target })) => {
                if st == (*version, tgt(target)) {
                    nexts.push(st);
                }
            }
            (
                RefOp::Cas { expect, target },
                Some(RefRet::Ok {
                    version,
                    target: t2,
                }),
            ) => {
                if st.0 == *expect && *version == expect + 1 && t2 == target {
                    nexts.push((expect + 1, tgt(target)));
                }
            }
            (RefOp::Cas { expect, .. }, Some(RefRet::Mismatch)) => {
                if st.0 != *expect {
                    nexts.push(st);
                }
            }
            (RefOp::Cas { expect, target }, None) => {
                if st.0 == *expect {
                    nexts.push((expect + 1, tgt(target)));
                }
            }
            (RefOp::Land { .. }, Some(RefRet::Ok { version, target })) => {
                if *version >= 1 && st.0 == version - 1 {
                    nexts.push((*version, tgt(target)));
                }
            }
            (RefOp::Land { .. }, None) => {
                for &x in targets {
                    nexts.push((st.0 + 1, x));
                }
            }
            _ => {}
        }
        for n in nexts {
            done[i] = true;
            if dfs(ops, inc, done, n, targets) {
                done[i] = false;
                return true;
            }
            done[i] = false;
        }
    }
    false
}

fn to_events(ops: &[Op]) -> Vec<Event> {
    let mut ev = Vec::new();
    for (i, o) in ops.iter().enumerate() {
        ev.push(Event {
            op: i as u64,
            client: i as u32,
            r: "main".into(),
            call: Some(o.call.clone()),
            ret: None,
            at: o.inv,
        });
        ev.push(Event {
            op: i as u64,
            client: i as u32,
            r: "main".into(),
            call: None,
            ret: Some(o.ret.clone().unwrap_or(RefRet::Unknown)),
            at: o.resp,
        });
    }
    ev.sort_by_key(|e| e.at);
    ev
}

/// A history from a real atomic execution, optionally mutated.
fn gen_history(rng: &mut ChaCha8Rng) -> Vec<Op> {
    let n = rng.random_range(1..=7);
    let mut st: (u64, u8) = (rng.random_range(0..3), rng.random_range(0..3));
    // (linearization point, index)
    let mut plan: Vec<(u64, u64, u64, RefOp, bool, bool)> = Vec::new();
    for _ in 0..n {
        let inv = rng.random_range(0..30u64);
        let lp = inv + rng.random_range(1..6u64);
        let resp = lp + rng.random_range(1..6u64);
        let call = match rng.random_range(0..3) {
            0 => RefOp::Read,
            1 => RefOp::Cas {
                expect: st.0 + rng.random_range(0..2u64),
                target: t(rng.random_range(0..3)),
            },
            _ => RefOp::Land {
                change: [rng.random(); 16],
            },
        };
        let lost = rng.random_bool(0.15);
        let fail = !lost && rng.random_bool(0.05);
        plan.push((lp, inv, resp, call, lost, fail));
    }
    plan.sort_by_key(|p| p.0);
    let mut ops = Vec::new();
    for (_, inv, resp, call, lost, fail) in plan {
        let applies = !fail && !(lost && rng.random_bool(0.5));
        let ret = if !applies {
            if fail { Some(RefRet::Fail) } else { None }
        } else {
            let r = match &call {
                RefOp::Read => RefRet::Value {
                    version: st.0,
                    target: t(st.1),
                },
                RefOp::Cas { expect, target } => {
                    if *expect == st.0 {
                        st = (st.0 + 1, tgt(target));
                        RefRet::Ok {
                            version: st.0,
                            target: *target,
                        }
                    } else {
                        RefRet::Mismatch
                    }
                }
                RefOp::Land { .. } => {
                    st = (st.0 + 1, rng.random_range(0..3));
                    RefRet::Ok {
                        version: st.0,
                        target: t(st.1),
                    }
                }
            };
            if lost { None } else { Some(r) }
        };
        ops.push(Op {
            call,
            ret,
            inv,
            resp,
        });
    }
    if rng.random_bool(0.5) && !ops.is_empty() {
        let i = rng.random_range(0..ops.len());
        let o = &mut ops[i];
        o.ret = match o.ret.take() {
            Some(RefRet::Value { version, target }) => Some(if rng.random_bool(0.5) {
                RefRet::Value {
                    version: version + 1,
                    target,
                }
            } else {
                RefRet::Value {
                    version,
                    target: t((tgt(&target) + 1) % 3),
                }
            }),
            Some(RefRet::Ok { .. }) if matches!(o.call, RefOp::Cas { .. }) => {
                Some(RefRet::Mismatch)
            }
            Some(RefRet::Mismatch) => match &o.call {
                RefOp::Cas { expect, target } => Some(RefRet::Ok {
                    version: expect + 1,
                    target: *target,
                }),
                _ => Some(RefRet::Mismatch),
            },
            Some(RefRet::Ok { version, target }) => Some(RefRet::Ok {
                version: version + 1,
                target,
            }),
            other => other,
        };
    }
    ops
}

#[test]
fn checker_agrees_with_brute_force() {
    let cases = scion_core::test_support::proptest_cases(20000);
    let mut rng = ChaCha8Rng::seed_from_u64(0x5c10);
    let (mut yes, mut no) = (0, 0);
    for case in 0..cases {
        let ops = gen_history(&mut rng);
        let expected = oracle(&ops);
        let got = check_ref_registers(&to_events(&ops));
        if let Err(v) = &got {
            assert!(!v.detail.contains("inconclusive"), "case {case}: {v}");
        }
        assert_eq!(
            got.is_ok(),
            expected,
            "case {case}: {ops:#?}\nchecker: {got:?}"
        );
        if expected {
            yes += 1;
        } else {
            no += 1;
        }
    }
    eprintln!("cases {cases}: linearizable {yes}, not {no}");
    // Both outcomes must be well represented for the comparison to mean anything.
    assert!(yes > cases / 5 && no > cases / 10, "yes {yes} no {no}");
}
