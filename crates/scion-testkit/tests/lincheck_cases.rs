//! SPEC 19.5 linearizability checker: hand-built accept/reject cases and randomized histories
//! produced by a correct (atomic) register, with and without Unknown completions.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test helpers

use parking_lot::Mutex;
use rand::{Rng, SeedableRng};
use rand_chacha::ChaCha8Rng;
use scion_testkit::lincheck::{
    Event, HistoryRecorder, MAX_OPS_PER_REF, RefOp, RefRet, check_ref_registers,
    check_ref_registers_with_initial,
};
use std::collections::BTreeMap;
use std::sync::Arc;

fn t(n: u8) -> [u8; 34] {
    let mut x = [0u8; 34];
    x[0] = 1;
    x[1] = 3;
    x[2] = n;
    x
}

/// Builder for hand-written histories with explicit timestamps.
#[derive(Default)]
struct H {
    events: Vec<Event>,
    next: u64,
}

impl H {
    /// An operation on `r` by `client`, invoked at `from` and completed at `to`.
    fn op(&mut self, client: u32, r: &str, from: u64, to: Option<u64>, call: RefOp, ret: RefRet) {
        let id = self.next;
        self.next += 1;
        self.events.push(Event {
            op: id,
            client,
            r: r.to_string(),
            call: Some(call),
            ret: None,
            at: from,
        });
        if let Some(to) = to {
            self.events.push(Event {
                op: id,
                client,
                r: r.to_string(),
                call: None,
                ret: Some(ret),
                at: to,
            });
        }
    }

    fn read(&mut self, c: u32, from: u64, to: u64, version: u64, target: u8) {
        self.op(
            c,
            "main",
            from,
            Some(to),
            RefOp::Read,
            RefRet::Value {
                version,
                target: t(target),
            },
        );
    }

    fn cas_ok(&mut self, c: u32, from: u64, to: u64, expect: u64, target: u8) {
        self.op(
            c,
            "main",
            from,
            Some(to),
            RefOp::Cas {
                expect,
                target: t(target),
            },
            RefRet::Ok {
                version: expect + 1,
                target: t(target),
            },
        );
    }

    fn cas_mismatch(&mut self, c: u32, from: u64, to: u64, expect: u64, target: u8) {
        self.op(
            c,
            "main",
            from,
            Some(to),
            RefOp::Cas {
                expect,
                target: t(target),
            },
            RefRet::Mismatch,
        );
    }

    fn cas_unknown(&mut self, c: u32, from: u64, to: u64, expect: u64, target: u8) {
        self.op(
            c,
            "main",
            from,
            Some(to),
            RefOp::Cas {
                expect,
                target: t(target),
            },
            RefRet::Unknown,
        );
    }

    fn land_ok(&mut self, c: u32, from: u64, to: u64, version: u64, target: u8) {
        self.op(
            c,
            "main",
            from,
            Some(to),
            RefOp::Land {
                change: [c as u8; 16],
            },
            RefRet::Ok {
                version,
                target: t(target),
            },
        );
    }

    fn land_unknown(&mut self, c: u32, from: u64, to: u64) {
        self.op(
            c,
            "main",
            from,
            Some(to),
            RefOp::Land {
                change: [c as u8; 16],
            },
            RefRet::Unknown,
        );
    }

    fn check(&self) -> Result<(), scion_testkit::lincheck::Violation> {
        check_ref_registers(&self.events)
    }
}

// ------------------------------------------------------------------------------------------
// Accepted histories
// ------------------------------------------------------------------------------------------

#[test]
fn accepts_sequential_history() {
    let mut h = H::default();
    h.read(1, 1, 2, 1, 1);
    h.cas_ok(1, 3, 4, 1, 2);
    h.read(2, 5, 6, 2, 2);
    h.cas_mismatch(2, 7, 8, 1, 3);
    h.land_ok(3, 9, 10, 3, 4);
    h.read(1, 11, 12, 3, 4);
    h.cas_ok(2, 13, 14, 3, 5);
    h.read(3, 15, 16, 4, 5);
    h.check().unwrap();
}

#[test]
fn accepts_concurrent_overlap_in_either_order() {
    // Two overlapping CAS at different versions whose only legal order is the reverse of their
    // invocation order.
    let mut h = H::default();
    h.read(1, 1, 2, 1, 1);
    h.cas_ok(1, 3, 10, 2, 3); // invoked first, must linearize second
    h.cas_ok(2, 4, 9, 1, 2);
    h.read(3, 11, 12, 3, 3);
    h.check().unwrap();

    // A read overlapping a CAS may see either the old or the new value.
    for seen in [(1u64, 1u8), (2, 2)] {
        let mut h = H::default();
        h.read(1, 1, 2, 1, 1);
        h.cas_ok(1, 3, 10, 1, 2);
        h.read(2, 4, 9, seen.0, seen.1);
        h.read(2, 11, 12, 2, 2);
        h.check().unwrap();
    }
}

#[test]
fn accepts_contended_cas_with_one_winner() {
    let mut h = H::default();
    h.read(0, 1, 2, 7, 1);
    // Five clients CAS at version 7 concurrently; exactly one wins.
    h.cas_mismatch(1, 3, 20, 7, 11);
    h.cas_mismatch(2, 4, 21, 7, 12);
    h.cas_ok(3, 5, 22, 7, 13);
    h.cas_mismatch(4, 6, 23, 7, 14);
    h.cas_mismatch(5, 7, 24, 7, 15);
    h.read(0, 25, 26, 8, 13);
    h.check().unwrap();
}

#[test]
fn accepts_unknown_completions_both_ways() {
    // Unknown CAS that took effect (a later read observes it).
    let mut h = H::default();
    h.read(1, 1, 2, 1, 1);
    h.cas_unknown(1, 3, 4, 1, 2);
    h.read(2, 5, 6, 2, 2);
    h.check().unwrap();

    // Unknown CAS that did not take effect.
    let mut h = H::default();
    h.read(1, 1, 2, 1, 1);
    h.cas_unknown(1, 3, 4, 1, 2);
    h.read(2, 5, 6, 1, 1);
    h.check().unwrap();

    // Unknown CAS that takes effect long after its client gave up (return at infinity).
    let mut h = H::default();
    h.read(1, 1, 2, 1, 1);
    h.cas_unknown(1, 3, 4, 1, 2);
    h.read(2, 5, 6, 1, 1);
    h.read(2, 7, 8, 1, 1);
    h.read(3, 9, 10, 2, 2);
    h.check().unwrap();

    // Never-completed operation (crashed client) counts as Unknown.
    let mut h = H::default();
    h.read(1, 1, 2, 1, 1);
    h.op(
        9,
        "main",
        3,
        None,
        RefOp::Cas {
            expect: 1,
            target: t(9),
        },
        RefRet::Unknown,
    );
    h.read(2, 5, 6, 2, 9);
    h.check().unwrap();

    // Unknown land: the version moves by one with a target only a later read reveals.
    let mut h = H::default();
    h.read(1, 1, 2, 4, 1);
    h.land_unknown(1, 3, 4);
    h.read(2, 5, 6, 5, 42);
    h.cas_ok(2, 7, 8, 5, 43);
    h.check().unwrap();
}

#[test]
fn accepts_independent_refs_interleaved() {
    let mut h = H::default();
    h.op(
        1,
        "main",
        1,
        Some(4),
        RefOp::Cas {
            expect: 1,
            target: t(2),
        },
        RefRet::Ok {
            version: 2,
            target: t(2),
        },
    );
    h.op(
        2,
        "release/1",
        2,
        Some(3),
        RefOp::Read,
        RefRet::Value {
            version: 9,
            target: t(9),
        },
    );
    h.op(
        2,
        "release/1",
        5,
        Some(6),
        RefOp::Cas {
            expect: 9,
            target: t(10),
        },
        RefRet::Ok {
            version: 10,
            target: t(10),
        },
    );
    h.check().unwrap();
}

#[test]
fn accepts_failures_without_effect() {
    let mut h = H::default();
    h.read(1, 1, 2, 1, 1);
    h.op(
        1,
        "main",
        3,
        Some(4),
        RefOp::Cas {
            expect: 1,
            target: t(2),
        },
        RefRet::Fail,
    );
    h.read(2, 5, 6, 1, 1);
    h.check().unwrap();
}

#[test]
fn known_initial_state_is_enforced() {
    let mut h = H::default();
    h.read(1, 1, 2, 3, 3);
    h.check().unwrap();
    let mut init = BTreeMap::new();
    init.insert("main".to_string(), (2u64, t(2)));
    assert!(check_ref_registers_with_initial(&h.events, &init).is_err());
    init.insert("main".to_string(), (3u64, t(3)));
    check_ref_registers_with_initial(&h.events, &init).unwrap();
}

// ------------------------------------------------------------------------------------------
// Rejected histories
// ------------------------------------------------------------------------------------------

#[test]
fn rejects_stale_read_after_completed_cas() {
    let mut h = H::default();
    h.read(1, 1, 2, 1, 1);
    h.cas_ok(1, 3, 4, 1, 2);
    h.read(2, 5, 6, 1, 1); // invoked after the CAS completed
    let v = h.check().unwrap_err();
    assert_eq!(v.r, "main");
    assert!(v.detail.contains("not linearizable"), "{}", v.detail);
}

#[test]
fn rejects_two_successful_cas_at_same_version() {
    // Sequential.
    let mut h = H::default();
    h.cas_ok(1, 1, 2, 1, 2);
    h.cas_ok(2, 3, 4, 1, 3);
    h.check().unwrap_err();
    // Concurrent: neither order is legal.
    let mut h = H::default();
    h.cas_ok(1, 1, 10, 1, 2);
    h.cas_ok(2, 2, 9, 1, 3);
    h.check().unwrap_err();
}

#[test]
fn rejects_version_gap() {
    let mut h = H::default();
    h.cas_ok(1, 1, 2, 1, 2);
    h.read(2, 3, 4, 4, 2); // nothing produced version 3 or 4
    h.check().unwrap_err();

    // A land that skips a version.
    let mut h = H::default();
    h.read(1, 1, 2, 5, 1);
    h.land_ok(1, 3, 4, 7, 2);
    h.check().unwrap_err();
}

#[test]
fn rejects_mismatch_when_version_matched() {
    let mut h = H::default();
    h.read(1, 1, 2, 3, 1);
    h.cas_mismatch(1, 3, 4, 3, 2); // nothing changed the ref in between
    h.check().unwrap_err();
}

#[test]
fn rejects_read_of_wrong_target() {
    let mut h = H::default();
    h.cas_ok(1, 1, 2, 1, 2);
    h.read(2, 3, 4, 2, 9); // right version, wrong target
    h.check().unwrap_err();
}

#[test]
fn rejects_unknown_that_cannot_explain_observation() {
    // An unknown CAS at the wrong version cannot produce version 2.
    let mut h = H::default();
    h.read(1, 1, 2, 1, 1);
    h.cas_unknown(1, 3, 4, 5, 2);
    h.read(2, 5, 6, 2, 2);
    h.check().unwrap_err();
}

#[test]
fn rejects_malformed_histories() {
    // Completion without invocation.
    let events = vec![Event {
        op: 1,
        client: 1,
        r: "main".into(),
        call: None,
        ret: Some(RefRet::Mismatch),
        at: 1,
    }];
    assert!(
        check_ref_registers(&events)
            .unwrap_err()
            .detail
            .contains("malformed")
    );
    // A read that returned Ok.
    let mut h = H::default();
    h.op(
        1,
        "main",
        1,
        Some(2),
        RefOp::Read,
        RefRet::Ok {
            version: 1,
            target: t(1),
        },
    );
    assert!(h.check().unwrap_err().detail.contains("malformed"));
}

#[test]
fn rejects_histories_above_the_cap_as_inconclusive() {
    let mut h = H::default();
    let mut at = 1;
    for i in 0..(MAX_OPS_PER_REF as u64 + 1) {
        h.read(1, at, at + 1, 1, 1);
        at += 2;
        let _ = i;
    }
    let v = h.check().unwrap_err();
    assert!(v.detail.contains("inconclusive"), "{}", v.detail);
}

// ------------------------------------------------------------------------------------------
// Randomized histories from an atomic register (must always pass) and mutated ones
// ------------------------------------------------------------------------------------------

/// A correct versioned register guarded by a mutex; operations take effect atomically
/// between `invoke` and `complete`.
struct Register {
    state: Mutex<(u64, [u8; 34])>,
}

impl Register {
    fn apply(&self, op: &RefOp, rng: &mut ChaCha8Rng) -> RefRet {
        let mut s = self.state.lock();
        match op {
            RefOp::Read => RefRet::Value {
                version: s.0,
                target: s.1,
            },
            RefOp::Cas { expect, target } => {
                if *expect == s.0 {
                    *s = (s.0 + 1, *target);
                    RefRet::Ok {
                        version: s.0,
                        target: s.1,
                    }
                } else {
                    RefRet::Mismatch
                }
            }
            RefOp::Land { .. } => {
                let target = t(rng.random());
                *s = (s.0 + 1, target);
                RefRet::Ok {
                    version: s.0,
                    target,
                }
            }
        }
    }
}

/// Run `clients` threads issuing `per_client` random operations against one register,
/// recording a history. With `unknown_p`, some completions are recorded as Unknown (the effect
/// still happened, or for CAS sometimes was skipped entirely, modelling a request lost before
/// reaching the server).
fn concurrent_history(seed: u64, clients: u32, per_client: u32, unknown_p: f64) -> Vec<Event> {
    let rec = Arc::new(HistoryRecorder::new());
    let reg = Arc::new(Register {
        state: Mutex::new((1, t(1))),
    });
    let mut handles = Vec::new();
    for c in 0..clients {
        let rec = rec.clone();
        let reg = reg.clone();
        handles.push(std::thread::spawn(move || {
            let mut rng = ChaCha8Rng::seed_from_u64(seed * 1000 + c as u64);
            let mut known_version = 1u64;
            for _ in 0..per_client {
                let op = match rng.random_range(0..10) {
                    0..=3 => RefOp::Read,
                    4..=7 => RefOp::Cas {
                        expect: known_version,
                        target: t(rng.random()),
                    },
                    _ => RefOp::Land {
                        change: rng.random(),
                    },
                };
                let tok = rec.invoke(c, "main", op.clone());
                std::thread::yield_now();
                let lost = rng.random_bool(unknown_p);
                let skip_effect = lost && rng.random_bool(0.5) && !matches!(op, RefOp::Read);
                let ret = if skip_effect {
                    RefRet::Unknown
                } else {
                    let r = reg.apply(&op, &mut rng);
                    if let RefRet::Value { version, .. } | RefRet::Ok { version, .. } = &r {
                        known_version = *version;
                    }
                    if lost { RefRet::Unknown } else { r }
                };
                std::thread::yield_now();
                rec.complete(tok, ret);
            }
        }));
    }
    for h in handles {
        h.join().unwrap();
    }
    rec.events()
}

#[test]
fn accepts_recorded_concurrent_histories() {
    let seeds = scion_core::test_support::proptest_cases(40) as u64;
    for seed in 0..seeds {
        let ev = concurrent_history(seed, 4, 12, 0.0);
        check_ref_registers(&ev).unwrap_or_else(|v| panic!("seed {seed}: {v}"));
    }
}

#[test]
fn accepts_recorded_histories_with_unknowns() {
    let seeds = scion_core::test_support::proptest_cases(40) as u64;
    for seed in 0..seeds {
        let ev = concurrent_history(seed, 5, 10, 0.2);
        check_ref_registers(&ev).unwrap_or_else(|v| panic!("seed {seed}: {v}"));
    }
}

#[test]
fn rejects_recorded_histories_with_a_forged_stale_read() {
    // Append a read, invoked after everything completed, that returns an older state.
    let seeds = scion_core::test_support::proptest_cases(20) as u64;
    for seed in 0..seeds {
        let mut ev = concurrent_history(seed, 3, 10, 0.0);
        let mut last_version = 0;
        let mut first_target = None;
        for e in &ev {
            match &e.ret {
                Some(RefRet::Ok { version, .. }) | Some(RefRet::Value { version, .. }) => {
                    last_version = last_version.max(*version);
                }
                _ => {}
            }
            if let Some(RefRet::Value { version: 1, target }) = &e.ret {
                first_target = Some(*target);
            }
        }
        if last_version < 2 {
            continue;
        }
        let at = ev.last().map_or(0, |e| e.at);
        let op = 1_000_000;
        ev.push(Event {
            op,
            client: 99,
            r: "main".into(),
            call: Some(RefOp::Read),
            ret: None,
            at: at + 1,
        });
        ev.push(Event {
            op,
            client: 99,
            r: "main".into(),
            call: None,
            ret: Some(RefRet::Value {
                version: 1,
                target: first_target.unwrap_or(t(1)),
            }),
            at: at + 2,
        });
        assert!(
            check_ref_registers(&ev).is_err(),
            "seed {seed} accepted a stale read"
        );
    }
}

#[test]
fn checks_a_history_near_the_recommended_cap() {
    // About 200 operations by 6 clients with unknowns: must finish and pass.
    let ev = concurrent_history(7, 6, 33, 0.1);
    let ops = ev.iter().filter(|e| e.call.is_some()).count();
    assert!(ops >= 190);
    check_ref_registers(&ev).unwrap();
}

/// 120 operations whose intervals all overlap: a CAS chain 1 -> 41 in shuffled invocation
/// order, a read of every version, and 40 losing CAS. With `bogus`, one read reports a version
/// nothing produced, so the history must be rejected after an exhaustive search.
fn overlapping(seed: u64, bogus: bool) -> H {
    let mut rng = ChaCha8Rng::seed_from_u64(seed);
    let mut ops: Vec<(RefOp, RefRet)> = Vec::new();
    for v in 1..=40u64 {
        ops.push((
            RefOp::Cas {
                expect: v,
                target: t(v as u8),
            },
            RefRet::Ok {
                version: v + 1,
                target: t(v as u8),
            },
        ));
        ops.push((
            RefOp::Read,
            RefRet::Value {
                version: v + 1,
                target: t(v as u8),
            },
        ));
        ops.push((
            RefOp::Cas {
                expect: 1000 + v,
                target: t(200),
            },
            RefRet::Mismatch,
        ));
    }
    if bogus {
        ops.push((
            RefOp::Read,
            RefRet::Value {
                version: 77,
                target: t(77),
            },
        ));
    }
    for i in (1..ops.len()).rev() {
        let j = rng.random_range(0..=i);
        ops.swap(i, j);
    }
    let mut h = H::default();
    let n = ops.len() as u64;
    for (i, (call, ret)) in ops.into_iter().enumerate() {
        let i = i as u64;
        h.op(i as u32, "main", 1 + i, Some(10 * n - i), call, ret);
    }
    h
}

#[test]
fn fully_overlapping_history_stays_tractable() {
    for seed in 0..5 {
        overlapping(seed, false).check().unwrap();
        let v = overlapping(seed, true).check().unwrap_err();
        assert!(v.detail.contains("not linearizable"), "{}", v.detail);
    }
}

#[test]
#[ignore = "spec-sized: run with SCION_FULL=1 scripts/check.sh --full"]
fn full_many_seeds_near_cap() {
    let seeds = if scion_core::test_support::full_mode() {
        1000
    } else {
        10
    };
    for seed in 0..seeds {
        let ev = concurrent_history(seed, 8, 25, 0.1);
        check_ref_registers(&ev).unwrap_or_else(|v| panic!("seed {seed}: {v}"));
    }
}
