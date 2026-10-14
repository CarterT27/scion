//! Storage layer (SPEC 3.2 level 2, SPEC 5; PLAN 2.3).
//!
//! - [`MetaStore`] / [`BlobStore`]: the backend traits. Every `MetaStore` method is exactly one
//!   serializable transaction (`docs/contracts/storage-semantics.md`).
//! - [`MemMetaStore`] / [`MemBlobStore`]: faithful hermetic in-memory backends.
//! - [`Storage`]: the bundle of both backends wrapped in counting/fault decorators.
//! - [`ObjectRepo`]: the object facade implementing SPEC 5.1, 5.3 step 1 and 5.4.
//! - [`fsck`]: the SPEC 4.5 invariant oracle; [`conformance`]: shared backend suites.
#![warn(missing_docs)]

pub mod conformance;

mod blob;
mod fsck;
mod instrument;
mod keys;
mod mem_blob;
mod mem_meta;
mod meta;
mod repo;
mod storage;
mod types;

pub use blob::BlobStore;
pub use fsck::{FsckOptions, Violation, fsck};
pub use instrument::{CounterSnapshot, Counters, InstrumentedBlob, InstrumentedMeta};
pub use keys::object_key;
pub use mem_blob::MemBlobStore;
pub use mem_meta::{MemMetaStore, MemOptions};
pub use meta::MetaStore;
pub use repo::{ObjectRepo, ObjectRepoConfig, StoredFrame};
pub use storage::{MemHandles, Storage};
pub use types::{
    Area, AuditRow, CasOutcome, ChangesetBundle, ChangesetRow, Consistency, DerivedKey, DerivedRow,
    InsertOutcome, LandedEntry, LandedRow, ObjectPathRow, ObjectRow, ObjectState, RedactTxn,
    RedactionRow, RedactionState, RefCas, RefExpect, RefLogRow, RefRow, RepoId, TableCounts,
};
