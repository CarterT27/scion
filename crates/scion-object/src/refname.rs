//! SPEC 4.3.6 ref names.
#![allow(unused_variables)] // P00 stub: remove when implemented.

use scion_core::Result;

/// A ref name: `^[a-z0-9][a-z0-9._/-]{0,199}$`, no `..`, no `//`, no trailing `/`.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RefName(String);

impl RefName {
    /// Validate a ref name (INVALID_ARGUMENT/invalid_ref_name).
    pub fn parse(s: &str) -> Result<Self> {
        todo!("P01")
    }

    /// The name.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The REQUIRED ref `main` (SPEC 4.3.6).
    pub fn main() -> Self {
        RefName("main".to_owned())
    }
}

impl std::fmt::Display for RefName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
