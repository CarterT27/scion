//! History Recorder and WGL linearizability checker for ref registers (SPEC 19.0, 19.5; P09).
//!
//! # Model
//!
//! Each ref is an independent *versioned register* holding `(version, target)` (SPEC 4.3.6:
//! the version increments by exactly 1 on every update):
//!
//! - `Read` returns `Value { version, target }`: the current state.
//! - `Cas { expect, target }` succeeds (`Ok { version: expect + 1, target }`) iff `expect`
//!   equals the current version; otherwise it returns `Mismatch` and has no effect. Creating a
//!   ref is a CAS with `expect = 0` (absent).
//! - `Land { change }` always succeeds when it takes effect: `Ok { version: v + 1, target }`
//!   where `target` is the new (rebased) changeset, which the model cannot predict.
//! - `Fail` is a definite failure with no effect; `Unknown` (crash, timeout, lost response)
//!   may or may not have taken effect. A missing completion counts as `Unknown`.
//!
//! The initial state of each ref is unknown unless given to
//! [`check_ref_registers_with_initial`]. An unknown initial version is inferred: every
//! observed version bounds it (it lies within the number of possible increments below the
//! smallest observation and above the largest), and each candidate in that range is searched,
//! most likely (the smallest observation) first. The initial target is unknown until read.
//!
//! # Algorithm
//!
//! The history is partitioned per ref (registers are independent, so a history is
//! linearizable iff every per-ref sub-history is). Each sub-history is checked with the
//! Wing-Gong-Lowe search: repeatedly linearize a minimal pending operation whose effect is
//! legal in the current model state, backtracking on failure, and memoize visited
//! `(linearized set, state)` pairs. Read-only operations (reads, CAS mismatches) are
//! linearized greedily as soon as they are legal, which keeps heavily contended CAS
//! histories tractable. `Unknown` operations have their return at infinity and are
//! optional: the search succeeds once every operation with a definite outcome is linearized.
//!
//! # Cap
//!
//! The search is exponential in the worst case. Keep each per-ref history at about 200
//! operations ([`RECOMMENDED_OPS_PER_REF`]) and run many seeds instead of long histories.
//! Sub-histories with more than [`MAX_OPS_PER_REF`] operations (after dropping no-op `Fail`
//! completions and unknown reads) are rejected as inconclusive, and the search gives up after
//! [`MAX_SEARCH_STEPS`] steps, also reported as a violation that says it is inconclusive.

use parking_lot::Mutex;
use std::collections::{BTreeMap, HashSet};

/// Recommended upper bound of operations per ref in one checked history (PLAN 3.4).
pub const RECOMMENDED_OPS_PER_REF: usize = 200;
/// Hard cap: per-ref sub-histories above this size are rejected as inconclusive.
pub const MAX_OPS_PER_REF: usize = 256;
/// Search budget per ref (steps of the WGL loop) before giving up as inconclusive.
pub const MAX_SEARCH_STEPS: u64 = 20_000_000;

/// An operation on a ref (SPEC 19.5: ResolveRef, UpdateRef, Land).
#[derive(Clone, Debug)]
pub enum RefOp {
    /// Read `(version, target)`.
    Read,
    /// CAS from version `expect` to `target`.
    Cas {
        /// Expected version.
        expect: u64,
        /// New target.
        target: [u8; 34],
    },
    /// Land a change (version + 1).
    Land {
        /// Change id.
        change: [u8; 16],
    },
}

/// An operation result (SPEC 19.5).
#[derive(Clone, Debug)]
pub enum RefRet {
    /// A read value.
    Value {
        /// Version.
        version: u64,
        /// Target.
        target: [u8; 34],
    },
    /// CAS lost.
    Mismatch,
    /// CAS or land succeeded.
    Ok {
        /// New version.
        version: u64,
        /// New target.
        target: [u8; 34],
    },
    /// Definite failure (no effect).
    Fail,
    /// Unknown (crash/timeout): may or may not have taken effect.
    Unknown,
}

/// Handle of an invoked operation (SPEC 19.0 History Recorder).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OpToken(pub u64);

/// One recorded invocation or completion (SPEC 19.0 History Recorder).
#[derive(Clone, Debug)]
pub struct Event {
    /// Operation id.
    pub op: u64,
    /// Client id.
    pub client: u32,
    /// Ref name.
    pub r: String,
    /// The operation (on invoke).
    pub call: Option<RefOp>,
    /// The result (on completion).
    pub ret: Option<RefRet>,
    /// Monotonic timestamp.
    pub at: u64,
}

/// A linearizability violation (SPEC 19.5).
#[derive(Clone, Debug)]
pub struct Violation {
    /// The ref whose sub-history is not linearizable.
    pub r: String,
    /// Explanation.
    pub detail: String,
}

impl std::fmt::Display for Violation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "ref {:?}: {}", self.r, self.detail)
    }
}

impl std::error::Error for Violation {}

#[derive(Default)]
struct RecorderInner {
    events: Vec<Event>,
    /// op id -> (client, ref) for open invocations.
    open: BTreeMap<u64, (u32, String)>,
    next_op: u64,
    clock: u64,
}

/// Thread-safe recorder with monotonic timestamps (SPEC 19.0 History Recorder).
///
/// Timestamps come from a logical clock advanced under the recorder's lock, so they are
/// strictly increasing and consistent with real time: if `complete(a)` returns before
/// `invoke(b)` is called, `a`'s completion timestamp is smaller than `b`'s invocation's.
/// Call `invoke` immediately before issuing a request and `complete` immediately after its
/// response (or failure) is observed.
#[derive(Default)]
pub struct HistoryRecorder {
    inner: Mutex<RecorderInner>,
}

impl HistoryRecorder {
    /// An empty recorder.
    pub fn new() -> Self {
        HistoryRecorder::default()
    }

    /// Record an invocation.
    pub fn invoke(&self, client: u32, r: &str, op: RefOp) -> OpToken {
        let mut g = self.inner.lock();
        let id = g.next_op;
        g.next_op += 1;
        g.clock += 1;
        let at = g.clock;
        g.open.insert(id, (client, r.to_string()));
        g.events.push(Event {
            op: id,
            client,
            r: r.to_string(),
            call: Some(op),
            ret: None,
            at,
        });
        OpToken(id)
    }

    /// Record a completion. Completing an unknown or already completed token is ignored
    /// (and logged).
    pub fn complete(&self, t: OpToken, ret: RefRet) {
        let mut g = self.inner.lock();
        let Some((client, r)) = g.open.remove(&t.0) else {
            tracing::warn!(
                op = t.0,
                "HistoryRecorder: completion for an unknown or closed op"
            );
            return;
        };
        g.clock += 1;
        let at = g.clock;
        g.events.push(Event {
            op: t.0,
            client,
            r,
            call: None,
            ret: Some(ret),
            at,
        });
    }

    /// Every recorded event in timestamp order.
    pub fn events(&self) -> Vec<Event> {
        self.inner.lock().events.clone()
    }
}

/// WGL search, partitioned per ref; `Unknown` completions may or may not have happened
/// (SPEC 19.5). The initial state of every ref is unknown and inferred from the history.
/// Histories are capped at about 200 operations per ref (see the module docs).
pub fn check_ref_registers(events: &[Event]) -> Result<(), Violation> {
    check_ref_registers_with_initial(events, &BTreeMap::new())
}

/// [`check_ref_registers`] with known initial `(version, target)` states for some refs
/// (SPEC 19.5). Refs absent from `initial` start in an unknown state.
pub fn check_ref_registers_with_initial(
    events: &[Event],
    initial: &BTreeMap<String, (u64, [u8; 34])>,
) -> Result<(), Violation> {
    let ops = pair_events(events)?;
    let mut per_ref: BTreeMap<&str, Vec<&Op>> = BTreeMap::new();
    for op in &ops {
        per_ref.entry(op.r.as_str()).or_default().push(op);
    }
    for (r, ops) in per_ref {
        let mut budget = MAX_SEARCH_STEPS;
        if let Some((v, t)) = initial.get(r) {
            let init = State {
                version: *v,
                target: Some(*t),
            };
            check_one(r, &ops, init, &mut budget)?;
            continue;
        }
        let Some(candidates) = initial_candidates(&ops) else {
            // Only mismatches and unknown operations: trivially linearizable.
            continue;
        };
        if candidates.is_empty() {
            return Err(Violation {
                r: r.to_string(),
                detail: "not linearizable: the observed versions differ by more than the \
                         number of operations that could have incremented the version \
                         (version gap)"
                    .to_string(),
            });
        }
        let mut last = None;
        for v0 in candidates {
            let init = State {
                version: v0,
                target: None,
            };
            match check_one(r, &ops, init, &mut budget) {
                Ok(()) => {
                    last = None;
                    break;
                }
                Err(v) if v.detail.starts_with("inconclusive") => return Err(v),
                Err(v) => last = Some(v),
            }
        }
        if let Some(v) = last {
            return Err(v);
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// History preparation
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Debug)]
struct Op {
    id: u64,
    client: u32,
    r: String,
    call: RefOp,
    /// `None` = unknown outcome (or never completed).
    ret: Option<RefRet>,
    invoke_at: u64,
    /// `u64::MAX` for unknown outcomes.
    return_at: u64,
}

fn malformed(r: &str, detail: String) -> Violation {
    Violation {
        r: r.to_string(),
        detail: format!("malformed history: {detail}"),
    }
}

/// Pair invocations with completions; validate call/return combinations.
fn pair_events(events: &[Event]) -> Result<Vec<Op>, Violation> {
    let mut sorted: Vec<&Event> = events.iter().collect();
    sorted.sort_by_key(|e| (e.at, e.ret.is_some()));
    let mut ops: BTreeMap<u64, Op> = BTreeMap::new();
    // Ops that already saw a completion (including `Unknown`, which leaves `ret` empty).
    let mut completed: HashSet<u64> = HashSet::new();
    for e in sorted {
        match (&e.call, &e.ret) {
            (Some(call), None) => {
                if ops.contains_key(&e.op) {
                    return Err(malformed(&e.r, format!("op {} invoked twice", e.op)));
                }
                ops.insert(
                    e.op,
                    Op {
                        id: e.op,
                        client: e.client,
                        r: e.r.clone(),
                        call: call.clone(),
                        ret: None,
                        invoke_at: e.at,
                        return_at: u64::MAX,
                    },
                );
            }
            (None, Some(ret)) => {
                let Some(op) = ops.get_mut(&e.op) else {
                    return Err(malformed(
                        &e.r,
                        format!("op {} completed but never invoked", e.op),
                    ));
                };
                if !completed.insert(e.op) {
                    return Err(malformed(&e.r, format!("op {} completed twice", e.op)));
                }
                if e.at <= op.invoke_at {
                    return Err(malformed(
                        &e.r,
                        format!("op {} completes before it starts", e.op),
                    ));
                }
                let ok = matches!(
                    (&op.call, ret),
                    (_, RefRet::Unknown)
                        | (_, RefRet::Fail)
                        | (RefOp::Read, RefRet::Value { .. })
                        | (RefOp::Cas { .. }, RefRet::Ok { .. })
                        | (RefOp::Cas { .. }, RefRet::Mismatch)
                        | (RefOp::Land { .. }, RefRet::Ok { .. })
                );
                if !ok {
                    return Err(malformed(
                        &e.r,
                        format!("op {} result {:?} does not fit {:?}", e.op, ret, op.call),
                    ));
                }
                if !matches!(ret, RefRet::Unknown) {
                    op.ret = Some(ret.clone());
                    op.return_at = e.at;
                }
                // `Unknown` keeps `ret = None` and the return at infinity.
            }
            _ => {
                return Err(malformed(
                    &e.r,
                    format!("event of op {} must carry exactly one of call or ret", e.op),
                ));
            }
        }
    }
    Ok(ops
        .into_values()
        .filter(|op| {
            // A definite failure has no effect and constrains nothing; an unknown read
            // observed nothing.
            let fail = matches!(op.ret, Some(RefRet::Fail));
            let unknown_read = op.ret.is_none() && matches!(op.call, RefOp::Read);
            !(fail || unknown_read)
        })
        .collect())
}

// ---------------------------------------------------------------------------------------------
// Model
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct State {
    version: u64,
    /// `None` until observed (unknown initial target, or after an unknown land).
    target: Option<[u8; 34]>,
}

/// The successor state of linearizing `op` in `s` (`None` = illegal here). Unknown outcomes
/// are modelled as "took effect" here; "never took effect" is the op staying unlinearized.
fn step(s: &State, op: &Op) -> Option<State> {
    match (&op.call, &op.ret) {
        (RefOp::Read, Some(RefRet::Value { version, target })) => {
            let target_ok = s.target.is_none_or(|t| t == *target);
            (s.version == *version && target_ok).then_some(State {
                version: *version,
                target: Some(*target),
            })
        }
        (RefOp::Cas { expect, target }, Some(RefRet::Ok { version, target: t })) => {
            (s.version == *expect && *version == expect.saturating_add(1) && t == target).then_some(
                State {
                    version: *version,
                    target: Some(*target),
                },
            )
        }
        (RefOp::Cas { expect, .. }, Some(RefRet::Mismatch)) => {
            (s.version != *expect).then(|| s.clone())
        }
        // Unknown CAS that took effect.
        (RefOp::Cas { expect, target }, None) => (s.version == *expect).then_some(State {
            version: expect.saturating_add(1),
            target: Some(*target),
        }),
        (RefOp::Land { .. }, Some(RefRet::Ok { version, target })) => {
            (*version >= 1 && s.version == version - 1).then_some(State {
                version: *version,
                target: Some(*target),
            })
        }
        // Unknown land that took effect: version + 1, unpredictable target.
        (RefOp::Land { .. }, None) => Some(State {
            version: s.version.saturating_add(1),
            target: None,
        }),
        _ => None,
    }
}

/// Candidate initial versions for a ref whose initial state is unknown, most likely first.
///
/// Every definite observation pins the version at its linearization point to a value `w`
/// (a read's version, a successful CAS's `expect`, a successful land's `version - 1`), and at
/// most `n` increments (successful or unknown CAS/land) precede it, so the initial version
/// lies in `[max(w) - n, min(w)]`. `None` means there is no observation at all, in which case
/// any initial version distinct from every CAS `expect` works (mismatches hold and unknown
/// operations are left out).
fn initial_candidates(ops: &[&Op]) -> Option<Vec<u64>> {
    let mut lo_obs: Option<u64> = None;
    let mut hi_obs: Option<u64> = None;
    let mut increments: u64 = 0;
    for op in ops {
        let obs = match (&op.call, &op.ret) {
            (RefOp::Read, Some(RefRet::Value { version, .. })) => Some(*version),
            (RefOp::Cas { expect, .. }, Some(RefRet::Ok { .. })) => {
                increments += 1;
                Some(*expect)
            }
            (RefOp::Land { .. }, Some(RefRet::Ok { version, .. })) => {
                increments += 1;
                Some(version.saturating_sub(1))
            }
            (RefOp::Cas { .. } | RefOp::Land { .. }, None) => {
                increments += 1;
                None
            }
            _ => None,
        };
        if let Some(w) = obs {
            lo_obs = Some(lo_obs.map_or(w, |x| x.min(w)));
            hi_obs = Some(hi_obs.map_or(w, |x| x.max(w)));
        }
    }
    let (min_w, max_w) = (lo_obs?, hi_obs?);
    let low = max_w.saturating_sub(increments);
    if low > min_w {
        return Some(Vec::new());
    }
    Some((low..=min_w).rev().collect())
}

// ---------------------------------------------------------------------------------------------
// WGL search
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Copy, Debug)]
struct Entry {
    /// Index into the op slice.
    op: usize,
    is_call: bool,
    /// Linked list (indices into the entry vector; `usize::MAX` = none).
    prev: usize,
    next: usize,
    /// For call entries: index of the matching return entry.
    mate: usize,
}

const NIL: usize = usize::MAX;

struct List {
    entries: Vec<Entry>,
    /// Sentinel head index (`entries.len() - 1` during construction; stored separately).
    head: usize,
}

impl List {
    fn build(ops: &[&Op]) -> List {
        // (time, is_return, op) ordering: at equal times calls come first (they cannot be equal
        // in recorder output, which is strictly monotonic).
        let mut points: Vec<(u64, bool, usize)> = Vec::with_capacity(ops.len() * 2);
        for (i, op) in ops.iter().enumerate() {
            points.push((op.invoke_at, false, i));
            points.push((op.return_at, true, i));
        }
        points.sort();
        let mut entries: Vec<Entry> = Vec::with_capacity(points.len() + 1);
        // Sentinel at index 0.
        entries.push(Entry {
            op: NIL,
            is_call: false,
            prev: NIL,
            next: NIL,
            mate: NIL,
        });
        let mut call_idx = vec![NIL; ops.len()];
        for (_, is_ret, i) in &points {
            let idx = entries.len();
            entries.push(Entry {
                op: *i,
                is_call: !is_ret,
                prev: idx - 1,
                next: NIL,
                mate: NIL,
            });
            if let Some(prev) = entries.get_mut(idx - 1) {
                prev.next = idx;
            }
            if *is_ret {
                if let Some(&c) = call_idx.get(*i)
                    && let Some(ce) = entries.get_mut(c)
                {
                    ce.mate = idx;
                }
            } else if let Some(slot) = call_idx.get_mut(*i) {
                *slot = idx;
            }
        }
        List { entries, head: 0 }
    }

    fn next(&self, i: usize) -> usize {
        self.entries.get(i).map_or(NIL, |e| e.next)
    }

    /// Unlink entry `i`.
    fn unlink(&mut self, i: usize) {
        let Some(e) = self.entries.get(i).copied() else {
            return;
        };
        if let Some(p) = self.entries.get_mut(e.prev) {
            p.next = e.next;
        }
        if let Some(n) = self.entries.get_mut(e.next) {
            n.prev = e.prev;
        }
    }

    /// Relink entry `i` (inverse of the most recent matching `unlink`).
    fn relink(&mut self, i: usize) {
        let Some(e) = self.entries.get(i).copied() else {
            return;
        };
        if let Some(p) = self.entries.get_mut(e.prev) {
            p.next = i;
        }
        if let Some(n) = self.entries.get_mut(e.next) {
            n.prev = i;
        }
    }

    /// Lift a call entry and its return entry out of the list.
    fn lift(&mut self, call: usize) {
        let mate = self.entries.get(call).map_or(NIL, |e| e.mate);
        self.unlink(call);
        self.unlink(mate);
    }

    /// Undo [`List::lift`].
    fn unlift(&mut self, call: usize) {
        let mate = self.entries.get(call).map_or(NIL, |e| e.mate);
        self.relink(mate);
        self.relink(call);
    }
}

struct Frame {
    call: usize,
    prev_state: State,
    /// A read-only op linearized greedily: skipping it instead can never help, so
    /// backtracking passes through this frame (see [`check_one`]).
    forced: bool,
}

/// Reads and CAS mismatches never change the version; they only observe it.
fn is_read_only(op: &Op) -> bool {
    matches!(
        (&op.call, &op.ret),
        (RefOp::Read, Some(RefRet::Value { .. })) | (RefOp::Cas { .. }, Some(RefRet::Mismatch))
    )
}

/// WGL search over one ref's operations.
///
/// Optimization: after every state change, all *read-only* operations (reads, CAS mismatches)
/// that are minimal (their call is reachable before any pending return) and legal in the
/// current state are linearized first, greedily, and are never "skipped" on backtrack. This
/// is sound: in any linearization of the remaining operations, such an operation can be moved
/// to the current point. Every operation that must precede it is already linearized, every
/// state change increments the version, so a read that matches the current version would see
/// the same target later in the same version window, and moving a read-only operation does
/// not alter the state any other operation observes.
fn check_one(r: &str, ops: &[&Op], init: State, budget: &mut u64) -> Result<(), Violation> {
    if ops.len() > MAX_OPS_PER_REF {
        return Err(Violation {
            r: r.to_string(),
            detail: format!(
                "inconclusive: {} operations exceed the cap of {MAX_OPS_PER_REF} per ref \
                 (keep histories at about {RECOMMENDED_OPS_PER_REF})",
                ops.len()
            ),
        });
    }
    let inconclusive = || Violation {
        r: r.to_string(),
        detail: format!(
            "inconclusive: search budget of {MAX_SEARCH_STEPS} steps exhausted ({} operations)",
            ops.len()
        ),
    };
    let mut list = List::build(ops);
    let words = ops.len().div_ceil(64).max(1);
    let mut linearized = vec![0u64; words];
    let mut cache: HashSet<(Vec<u64>, State)> = HashSet::new();
    let mut stack: Vec<Frame> = Vec::new();
    let mut state = init;
    let mut entry = list.next(list.head);
    let mut deepest: usize = 0;
    let mut scan = true;

    loop {
        if *budget == 0 {
            return Err(inconclusive());
        }
        *budget -= 1;

        let mut backtrack = false;
        if scan {
            // Phase 1: linearize one legal, minimal, read-only operation if there is one.
            scan = false;
            let mut cur = list.next(list.head);
            let mut found: Option<(usize, usize, State)> = None;
            while let Some(e) = list.entries.get(cur).copied() {
                if !e.is_call {
                    break;
                }
                if let Some(op) = ops.get(e.op).copied()
                    && is_read_only(op)
                    && let Some(next_state) = step(&state, op)
                {
                    found = Some((cur, e.op, next_state));
                    break;
                }
                if *budget == 0 {
                    return Err(inconclusive());
                }
                *budget -= 1;
                cur = e.next;
            }
            match found {
                Some((call, op_idx, next_state)) => {
                    let mut lin = linearized.clone();
                    set_bit(&mut lin, op_idx);
                    if cache.insert((lin.clone(), next_state.clone())) {
                        stack.push(Frame {
                            call,
                            prev_state: std::mem::replace(&mut state, next_state),
                            forced: true,
                        });
                        linearized = lin;
                        list.lift(call);
                        deepest = deepest.max(stack.len());
                        scan = true;
                        continue;
                    }
                    // The greedy successor was already explored and failed.
                    backtrack = true;
                }
                None => entry = list.next(list.head),
            }
        }

        if !backtrack {
            // Phase 2: the classic WGL step over state-changing operations.
            let Some(e) = list.entries.get(entry).copied() else {
                // End of list: every entry was lifted, so everything is linearized.
                return Ok(());
            };
            let op = ops.get(e.op).copied();
            if e.is_call {
                let Some(op) = op else {
                    return Ok(());
                };
                let mut advanced = false;
                if !is_read_only(op)
                    && let Some(next_state) = step(&state, op)
                {
                    let mut lin = linearized.clone();
                    set_bit(&mut lin, e.op);
                    if cache.insert((lin.clone(), next_state.clone())) {
                        stack.push(Frame {
                            call: entry,
                            prev_state: std::mem::replace(&mut state, next_state),
                            forced: false,
                        });
                        linearized = lin;
                        list.lift(entry);
                        deepest = deepest.max(stack.len());
                        scan = true;
                        advanced = true;
                    }
                }
                if !advanced {
                    entry = list.next(entry);
                }
            } else if op.is_some_and(|o| o.ret.is_none()) {
                // A return at infinity: every operation with a definite outcome has been
                // linearized (their returns all precede this entry), so the history is
                // linearizable; the remaining unknown operations simply never took effect.
                return Ok(());
            } else {
                // A definite return reached before its call was linearized.
                backtrack = true;
            }
        }

        if backtrack {
            loop {
                let Some(frame) = stack.pop() else {
                    return Err(Violation {
                        r: r.to_string(),
                        detail: describe_failure(ops, deepest),
                    });
                };
                let op_idx = list.entries.get(frame.call).map_or(NIL, |c| c.op);
                state = frame.prev_state;
                clear_bit(&mut linearized, op_idx);
                list.unlift(frame.call);
                if !frame.forced {
                    // Continue the walk after the undone op; this configuration's read-only
                    // operations were already exhausted when it was first reached.
                    entry = list.next(frame.call);
                    break;
                }
            }
        }
    }
}

fn set_bit(bits: &mut [u64], i: usize) {
    if let Some(w) = bits.get_mut(i / 64) {
        *w |= 1u64 << (i % 64);
    }
}

fn clear_bit(bits: &mut [u64], i: usize) {
    if let Some(w) = bits.get_mut(i / 64) {
        *w &= !(1u64 << (i % 64));
    }
}

fn describe_failure(ops: &[&Op], deepest: usize) -> String {
    let mut s = format!(
        "not linearizable: no order of the {} operations is legal for a versioned register \
         (deepest prefix linearized: {deepest}); operations by invocation time:",
        ops.len()
    );
    let mut sorted: Vec<&&Op> = ops.iter().collect();
    sorted.sort_by_key(|o| o.invoke_at);
    for o in sorted.iter().take(40) {
        let ret = match &o.ret {
            Some(r) => format!("{r:?}"),
            None => "Unknown".to_string(),
        };
        let ret_at = if o.return_at == u64::MAX {
            "inf".to_string()
        } else {
            o.return_at.to_string()
        };
        s.push_str(&format!(
            "\n  op {} client {} [{}, {}] {} -> {}",
            o.id,
            o.client,
            o.invoke_at,
            ret_at,
            short_call(&o.call),
            short_ret(&ret)
        ));
    }
    if sorted.len() > 40 {
        s.push_str(&format!("\n  ... {} more", sorted.len() - 40));
    }
    s
}

fn short_call(c: &RefOp) -> String {
    match c {
        RefOp::Read => "Read".to_string(),
        RefOp::Cas { expect, target } => format!("Cas(expect {expect}, {})", hex8(target)),
        RefOp::Land { change } => format!("Land({})", hex8(change)),
    }
}

fn short_ret(r: &str) -> String {
    if r.chars().count() > 80 {
        let head: String = r.chars().take(80).collect();
        format!("{head}...")
    } else {
        r.to_string()
    }
}

fn hex8(b: &[u8]) -> String {
    b.iter().take(4).map(|x| format!("{x:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(n: u8) -> [u8; 34] {
        [n; 34]
    }

    #[test]
    fn recorder_timestamps_are_monotonic() {
        let rec = HistoryRecorder::new();
        let a = rec.invoke(1, "main", RefOp::Read);
        let b = rec.invoke(2, "main", RefOp::Read);
        rec.complete(
            b,
            RefRet::Value {
                version: 1,
                target: t(1),
            },
        );
        rec.complete(
            a,
            RefRet::Value {
                version: 1,
                target: t(1),
            },
        );
        let ev = rec.events();
        assert_eq!(ev.len(), 4);
        assert!(ev.windows(2).all(|w| w[0].at < w[1].at));
        assert!(check_ref_registers(&ev).is_ok());
    }
}
