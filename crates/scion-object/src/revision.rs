//! SPEC 11.8 revision grammar (PLAN 6.6).
//!
//! ```text
//! revision := base suffix*
//! suffix   := "~" digits? | "^" digits?          // bare "~" / "^" mean 1; "^0" is invalid
//! base     := "git:" hex{40}                     // Git
//!           | refname "@" digits                 // RefAt (u64 version)
//!           | id-text                            // Id (full SPEC 4.2 text form, any kind)
//!           | "c1" hex{8,63}                     // IdPrefix (string keeps the "c1")
//!           | refname                            // Name: a ref, or a bare hex prefix (>= 8)
//! ```
//!
//! Hex digits are lowercase. Base forms are tried in the order listed, so a token that is
//! both a full id text (or a `c1` prefix) and a valid ref name parses as the id form.
//! Whether a [`RevBase::Name`] is a ref or a bare hex prefix is decided by the server (an
//! existing ref with that exact name wins; PLAN 6.6).

use crate::refname::RefName;
use scion_core::{ObjectId, Result, ScionError, subcode};

/// The base of a revision expression.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RevBase {
    /// A full id text.
    Id(ObjectId),
    /// `c1` + 8..63 hex. The string is the whole token, including the leading `c1`; use
    /// [`RevBase::hex_prefix`] for the digest digits.
    IdPrefix(String),
    /// A bare token: a ref name, or a bare hex prefix of at least 8 digits (decided by the
    /// server: an exact ref name wins).
    Name(String),
    /// `<ref>@<version>`.
    RefAt(RefName, u64),
    /// `git:<40 hex>`.
    Git(String),
}

impl RevBase {
    /// The lowercase digest-hex prefix to search changesets by (SPEC 4.2, PLAN 6.6): the
    /// digits after `c1` for [`RevBase::IdPrefix`], or the whole token for a
    /// [`RevBase::Name`] made of at least 8 (and at most 64) lowercase hex digits. `None`
    /// otherwise.
    pub fn hex_prefix(&self) -> Option<&str> {
        match self {
            RevBase::IdPrefix(s) => s.strip_prefix("c1"),
            RevBase::Name(s) if (MIN_PREFIX_HEX..=64).contains(&s.len()) && is_hex(s) => {
                Some(s.as_str())
            }
            _ => None,
        }
    }
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

/// SPEC 4.2: changeset prefixes have at least 8 hex digits.
const MIN_PREFIX_HEX: usize = 8;

fn is_hex(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

fn bad(s: &str, why: &str) -> ScionError {
    ScionError::invalid(
        subcode::INVALID_REVISION,
        format!("invalid revision {s:?}: {why}"),
    )
}

fn is_digits(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())
}

fn parse_base(s: &str, whole: &str) -> Result<RevBase> {
    if s.is_empty() {
        return Err(bad(whole, "empty base"));
    }
    if let Some(sha) = s.strip_prefix("git:") {
        if sha.len() == 40 && is_hex(sha) {
            return Ok(RevBase::Git(sha.to_owned()));
        }
        return Err(bad(whole, "git: needs 40 lowercase hex digits"));
    }
    if let Some((name, version)) = s.split_once('@') {
        let name = RefName::parse(name).map_err(|_| bad(whole, "bad ref name before '@'"))?;
        if !is_digits(version) {
            return Err(bad(whole, "'@' needs a decimal version"));
        }
        let v = version
            .parse::<u64>()
            .map_err(|_| bad(whole, "version out of range"))?;
        return Ok(RevBase::RefAt(name, v));
    }
    if s.len() >= 66
        && let Ok(id) = ObjectId::parse_text(s)
    {
        return Ok(RevBase::Id(id));
    }
    if let Some(hex) = s.strip_prefix("c1")
        && (MIN_PREFIX_HEX..64).contains(&hex.len())
        && is_hex(hex)
    {
        return Ok(RevBase::IdPrefix(s.to_owned()));
    }
    if RefName::parse(s).is_ok() {
        return Ok(RevBase::Name(s.to_owned()));
    }
    Err(bad(
        whole,
        "not an id, id prefix, ref name, <ref>@<version> or git:<sha>",
    ))
}

/// Parse a revision string (INVALID_ARGUMENT/invalid_revision).
pub fn parse_revision(s: &str) -> Result<Revision> {
    let split = s.find(['~', '^']).unwrap_or(s.len());
    let (base_str, mut rest) = s.split_at(split);
    let base = parse_base(base_str, s)?;
    let mut suffixes = Vec::new();
    while let Some(op) = rest.chars().next() {
        let after = rest.get(1..).unwrap_or_default();
        let end = after
            .find(|c: char| !c.is_ascii_digit())
            .unwrap_or(after.len());
        let (digits, tail) = after.split_at(end);
        let n = if digits.is_empty() {
            1
        } else {
            digits
                .parse::<u32>()
                .map_err(|_| bad(s, "suffix count out of range"))?
        };
        match op {
            '~' => suffixes.push(RevSuffix::Ancestor(n)),
            '^' => {
                if n == 0 {
                    return Err(bad(s, "^0 is invalid"));
                }
                suffixes.push(RevSuffix::Parent(n));
            }
            _ => return Err(bad(s, "expected '~' or '^'")),
        }
        rest = tail;
    }
    Ok(Revision { base, suffixes })
}
