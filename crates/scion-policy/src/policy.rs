//! Policy evaluation (SPEC 6.2, 12.4; P05).
#![allow(unused_variables, dead_code)] // P00 stub: remove when implemented.

use crate::error::PolicyError;
use scion_core::Principal;
use scion_object::{RefName, RepoPath};

/// SPEC 6.2 `[admin]` permissions.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum AdminPerm {
    /// `admin.redact`.
    Redact,
    /// `admin.purge`.
    Purge,
    /// `admin.refs` (create/delete refs).
    Refs,
}

/// A validated, normalized policy.
#[derive(Clone, Debug)]
pub struct Policy {
    epoch: u64,
}

impl Policy {
    /// Parse and validate `acl.toml` text.
    pub fn parse(toml: &str) -> Result<Self, PolicyError> {
        todo!("P05")
    }

    /// `BLAKE3(normalized policy)[0..8]` big-endian (PLAN 6.2).
    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    /// Longest matching read rule grants `p`.
    pub fn can_read(&self, p: &Principal, path: &RepoPath) -> bool {
        todo!("P05")
    }

    /// Longest matching write rule grants `p`.
    pub fn can_write(&self, p: &Principal, path: &RepoPath) -> bool {
        todo!("P05")
    }

    /// True when the longest matching read rule for `path` is `/` (root access class).
    pub fn read_governed_by_root(&self, path: &RepoPath) -> bool {
        todo!("P05")
    }

    /// `update` on ref `r` (exact name beats patterns; longest pattern wins).
    pub fn can_update_ref(&self, p: &Principal, r: &RefName) -> bool {
        todo!("P05")
    }

    /// `force` on ref `r`.
    pub fn can_force_ref(&self, p: &Principal, r: &RefName) -> bool {
        todo!("P05")
    }

    /// An `[admin]` permission.
    pub fn can_admin(&self, p: &Principal, a: AdminPerm) -> bool {
        todo!("P05")
    }

    /// Groups `p` belongs to (transitively), sorted.
    pub fn groups_of(&self, p: &Principal) -> Vec<String> {
        todo!("P05")
    }

    /// A policy granting everything to everyone (tests).
    pub fn allow_all_for_tests() -> Self {
        todo!("P05")
    }
}
