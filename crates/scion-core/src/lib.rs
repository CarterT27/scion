//! Shared vocabulary of the Scion workspace (SPEC 3.2 level 1, PLAN 2.1).
//!
//! `scion-core` holds the types every other crate speaks: the error model (SPEC 7.2), object
//! identifiers and their text form (SPEC 4.2), file modes (SPEC 4.3.2), conflict payloads
//! (SPEC 8.3), principals (SPEC 4.3.8), the injectable clock, limits (SPEC 6.5), and the fault
//! registry used by the failure-injection tests (SPEC 19.0).

pub mod clock;
pub mod conflict;
pub mod error;
pub mod fault;
pub mod id;
pub mod limits;
pub mod mode;
pub mod principal;
pub mod subcode;
pub mod test_support;

pub use clock::{Clock, ManualClock, SystemClock};
pub use conflict::{CleanEntry, Conflict, ConflictHunk, ConflictKind, ConflictReport, EntryInfo};
pub use error::{ErrorCode, ErrorPayload, GrpcCode, Result, ScionError};
pub use fault::{FaultAction, FaultRegistry, points, trace};
pub use id::{ALGO_BLAKE3, ChangeId, Kind, ObjectId, empty_tree_id};
pub use limits::Limits;
pub use mode::Mode;
pub use principal::{Principal, Tier};
