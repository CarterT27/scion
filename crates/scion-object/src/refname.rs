//! SPEC 4.3.6 ref names.

use scion_core::{Result, ScionError, subcode};

/// SPEC 4.3.6: maximum ref name length (1 leading character plus up to 199).
const MAX_REF_NAME_LEN: usize = 200;

/// A ref name: `^[a-z0-9][a-z0-9._/-]{0,199}$`, no `..`, no `//`, no trailing `/`.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RefName(String);

fn bad(s: &str, why: &str) -> ScionError {
    ScionError::invalid(
        subcode::INVALID_REF_NAME,
        format!("invalid ref name {s:?}: {why}"),
    )
}

impl RefName {
    /// Validate a ref name (INVALID_ARGUMENT/invalid_ref_name).
    pub fn parse(s: &str) -> Result<Self> {
        let b = s.as_bytes();
        let Some(first) = b.first() else {
            return Err(bad(s, "empty"));
        };
        if b.len() > MAX_REF_NAME_LEN {
            return Err(bad(s, "longer than 200 bytes"));
        }
        if !matches!(first, b'a'..=b'z' | b'0'..=b'9') {
            return Err(bad(s, "must start with [a-z0-9]"));
        }
        if !b
            .iter()
            .all(|c| matches!(c, b'a'..=b'z' | b'0'..=b'9' | b'.' | b'_' | b'/' | b'-'))
        {
            return Err(bad(s, "characters must be [a-z0-9._/-]"));
        }
        if s.contains("..") {
            return Err(bad(s, "contains '..'"));
        }
        if s.contains("//") {
            return Err(bad(s, "contains '//'"));
        }
        if s.ends_with('/') {
            return Err(bad(s, "ends with '/'"));
        }
        Ok(RefName(s.to_owned()))
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

impl std::str::FromStr for RefName {
    type Err = ScionError;
    /// Same as [`RefName::parse`] (SPEC 4.3.6).
    fn from_str(s: &str) -> Result<Self> {
        RefName::parse(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn main_is_valid() {
        assert_eq!(RefName::parse("main").unwrap(), RefName::main());
        assert_eq!(RefName::main().to_string(), "main");
    }
}
