//! Canonical encoding and object entities (SPEC 3.2 level 1; PLAN 2.2).
//!
//! - [`enc`]: the SPEC 4.1 primitive encoder/decoder.
//! - [`Canonical`]: encode/decode/id for structured objects.
//! - Entities: [`Tree`] (4.3.2), [`Meta`] (4.3.3), [`Changeset`] (4.3.4), [`HistoryNode`] (9.2).
//! - Names and paths (4.4), ref names (4.3.6), the revision grammar (11.8) and the pure
//!   FileDiff over flat trees (4.3.4).
//!
//! No IO happens in this crate.

pub mod enc;

mod canonical;
mod changeset;
mod extensions;
mod flat;
mod history_node;
mod meta;
mod path;
mod refname;
mod revision;
mod tree;

pub use canonical::Canonical;
pub use changeset::{Changeset, DeltaEntry, DeltaOp};
pub use extensions::{Extensions, META_EXT_GIT_COMMIT, META_EXT_SKIPPED_GITLINKS};
pub use flat::{FlatTree, file_diff};
pub use history_node::{HistoryNode, NodeKind};
pub use meta::{Meta, Timestamp};
pub use path::{Name, RepoPath};
pub use refname::RefName;
pub use revision::{RevBase, RevSuffix, Revision, parse_revision};
pub use tree::{Tree, TreeEntry};
