//! Test infrastructure (SPEC 19.0; PLAN 2.21): corpus generators, shared-subtree repositories,
//! edge-case repositories, production encoding of structural trees, vendored git histories, the
//! linearizability checker, and in-process fakes (upstream server, GitHub) plus test TLS.
#![warn(missing_docs)]

pub mod git;
pub mod lincheck;

mod corpus;
mod edge;
mod encode;
mod fake_github;
mod fake_upstream;
mod shared;
mod tls;

pub use corpus::{CorpusSpec, GenCommit, GenRepo, generate};
pub use edge::edge_cases;
pub use encode::encode_tree;
pub use fake_github::FakeGitHub;
pub use fake_upstream::{FakeUpstream, FakeUpstreamOpts, RecordedRequest};
pub use shared::{SharedSubtreeRepo, shared_subtree_repo};
pub use tls::TestTls;
