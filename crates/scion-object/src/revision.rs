//! SPEC 11.8 revision grammar (PLAN 6.6).
#![allow(unused_variables)] // P00 stub: remove when implemented.

use crate::refname::RefName;
use scion_core::{ObjectId, Result};

/// The base of a revision expression.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RevBase {
    /// A full id text.
    Id(ObjectId),
    /// `c1` + 8..63 hex.
    IdPrefix(String),
    /// A bare token: a ref name, or a bare hex prefix of at least 8 digits (decided by the
    /// server: an exact ref name wins).
    Name(String),
    /// `<ref>@<version>`.
    RefAt(RefName, u64),
    /// `git:<40 hex>`.
    Git(String),
}

/// A revision suffix.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RevSuffix {
    /// `~n`: n-th first-parent ancestor (bare `~` = 1).
    Ancestor(u32),
    /// `^k`: k-th parent, k >= 1 (bare `^` = 1).
    Parent(u32),
}

/// A parsed revision: base plus suffixes applied left to right.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Revision {
    /// The base.
    pub base: RevBase,
    /// Suffixes in order.
    pub suffixes: Vec<RevSuffix>,
}

/// Parse a revision string (INVALID_ARGUMENT/invalid_revision).
pub fn parse_revision(s: &str) -> Result<Revision> {
    todo!("P01")
}
