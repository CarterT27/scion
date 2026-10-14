//! History Recorder and WGL linearizability checker for ref registers (SPEC 19.0, 19.5; P09).
#![allow(unused_variables, dead_code)] // P00 stub: remove when implemented.

use parking_lot::Mutex;

/// An operation on a ref.
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

/// An operation result.
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

/// Handle of an invoked operation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OpToken(pub u64);

/// One recorded invocation or completion.
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

/// A linearizability violation.
#[derive(Clone, Debug)]
pub struct Violation {
    /// The ref whose sub-history is not linearizable.
    pub r: String,
    /// Explanation.
    pub detail: String,
}

/// Thread-safe recorder with monotonic timestamps.
#[derive(Default)]
pub struct HistoryRecorder {
    events: Mutex<Vec<Event>>,
}

impl HistoryRecorder {
    /// An empty recorder.
    pub fn new() -> Self {
        HistoryRecorder::default()
    }

    /// Record an invocation.
    pub fn invoke(&self, client: u32, r: &str, op: RefOp) -> OpToken {
        todo!("P09")
    }

    /// Record a completion.
    pub fn complete(&self, t: OpToken, ret: RefRet) {
        todo!("P09")
    }

    /// Every recorded event in timestamp order.
    pub fn events(&self) -> Vec<Event> {
        todo!("P09")
    }
}

/// WGL search, partitioned per ref; `Unknown` completions may or may not have happened.
/// Histories are capped at about 200 operations per ref.
pub fn check_ref_registers(events: &[Event]) -> Result<(), Violation> {
    todo!("P09")
}
