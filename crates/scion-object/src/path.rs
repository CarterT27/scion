//! SPEC 4.4 names and repository paths.

use scion_core::{Result, ScionError, subcode};

/// SPEC 4.4: maximum name length in bytes.
const MAX_NAME_BYTES: usize = 255;
/// SPEC 4.4: maximum path length in bytes.
const MAX_PATH_BYTES: usize = 4096;

fn invalid_path(msg: impl Into<String>) -> ScionError {
    ScionError::invalid(subcode::INVALID_PATH, msg)
}

/// SPEC 4.4 name check shared by [`Name::new`] and [`RepoPath::parse`].
fn check_name(b: &[u8]) -> Result<()> {
    if b.is_empty() {
        return Err(invalid_path("empty path component"));
    }
    if b.len() > MAX_NAME_BYTES {
        return Err(invalid_path(format!(
            "path component of {} bytes exceeds {MAX_NAME_BYTES}",
            b.len()
        )));
    }
    if b.contains(&0) {
        return Err(invalid_path("path component contains NUL"));
    }
    if b.contains(&b'/') {
        return Err(invalid_path("path component contains '/'"));
    }
    if b == b"." || b == b".." {
        return Err(invalid_path("path component is '.' or '..'"));
    }
    if b.eq_ignore_ascii_case(b".scion") {
        return Err(invalid_path("path component is reserved '.scion'"));
    }
    Ok(())
}

/// A single path component (SPEC 4.4): 1-255 bytes, no NUL or `/`, not `.` or `..`, not
/// `.scion` under ASCII case-insensitive comparison.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Name(Vec<u8>);

impl Name {
    /// Validate a name (INVALID_ARGUMENT/invalid_path).
    pub fn new(b: &[u8]) -> Result<Self> {
        check_name(b)?;
        Ok(Name(b.to_vec()))
    }

    /// The raw bytes.
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }
}

/// A repository path (SPEC 4.4): names joined by `/`, at most 4096 bytes, no leading or
/// trailing `/`; the root is the empty path. Ordering is bytewise.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RepoPath(Vec<u8>);

impl RepoPath {
    /// Validate a path (INVALID_ARGUMENT/invalid_path). The empty input is the root.
    pub fn parse(b: &[u8]) -> Result<Self> {
        if b.is_empty() {
            return Ok(RepoPath::root());
        }
        if b.len() > MAX_PATH_BYTES {
            return Err(invalid_path(format!(
                "path of {} bytes exceeds {MAX_PATH_BYTES}",
                b.len()
            )));
        }
        if b.first() == Some(&b'/') || b.last() == Some(&b'/') {
            return Err(invalid_path("path has a leading or trailing '/'"));
        }
        for c in b.split(|x| *x == b'/') {
            check_name(c)?;
        }
        Ok(RepoPath(b.to_vec()))
    }

    /// The root (empty) path.
    pub fn root() -> Self {
        RepoPath(Vec::new())
    }

    /// True for the root path.
    pub fn is_root(&self) -> bool {
        self.0.is_empty()
    }

    /// The raw bytes.
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    /// The parent directory; `None` for the root.
    pub fn parent(&self) -> Option<RepoPath> {
        if self.is_root() {
            return None;
        }
        match self.0.iter().rposition(|b| *b == b'/') {
            Some(i) => Some(RepoPath(self.0.get(..i).unwrap_or_default().to_vec())),
            None => Some(RepoPath::root()),
        }
    }

    /// The last component; `None` for the root.
    pub fn file_name(&self) -> Option<&[u8]> {
        if self.is_root() {
            return None;
        }
        match self.0.iter().rposition(|b| *b == b'/') {
            Some(i) => self.0.get(i + 1..),
            None => Some(&self.0),
        }
    }

    /// The last component as a [`Name`]; `None` for the root (SPEC 4.4).
    pub fn name(&self) -> Option<Name> {
        self.file_name().map(|b| Name(b.to_vec()))
    }

    /// The components in order (none for the root).
    pub fn components(&self) -> impl Iterator<Item = &[u8]> {
        self.0.split(|b| *b == b'/').filter(|c| !c.is_empty())
    }

    /// Number of components (0 for the root).
    pub fn depth(&self) -> usize {
        if self.is_root() {
            0
        } else {
            self.0.iter().filter(|b| **b == b'/').count() + 1
        }
    }

    /// All strict ancestors, root first, excluding `self`.
    pub fn ancestors(&self) -> Vec<RepoPath> {
        if self.is_root() {
            return Vec::new();
        }
        let mut out = vec![RepoPath::root()];
        for (i, b) in self.0.iter().enumerate() {
            if *b == b'/' {
                out.push(RepoPath(self.0.get(..i).unwrap_or_default().to_vec()));
            }
        }
        out
    }

    /// Strict, component-wise ancestry (`a` is an ancestor of `a/b`, not of `ab`).
    pub fn is_ancestor_of(&self, other: &RepoPath) -> bool {
        if self.is_root() {
            return !other.is_root();
        }
        other.0.len() > self.0.len()
            && other.0.starts_with(&self.0)
            && other.0.get(self.0.len()) == Some(&b'/')
    }

    /// `self/n` (INVALID_ARGUMENT/invalid_path when too long).
    pub fn join(&self, n: &Name) -> Result<RepoPath> {
        if self.is_root() {
            return Ok(RepoPath(n.0.clone()));
        }
        let len = self.0.len() + 1 + n.0.len();
        if len > MAX_PATH_BYTES {
            return Err(invalid_path(format!(
                "path of {len} bytes exceeds {MAX_PATH_BYTES}"
            )));
        }
        let mut v = Vec::with_capacity(len);
        v.extend_from_slice(&self.0);
        v.push(b'/');
        v.extend_from_slice(&n.0);
        Ok(RepoPath(v))
    }
}

impl std::fmt::Display for RepoPath {
    /// Lossy UTF-8 rendering for messages (never use it as an identity).
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&String::from_utf8_lossy(&self.0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> RepoPath {
        RepoPath::parse(s.as_bytes()).unwrap()
    }

    #[test]
    fn names() {
        assert!(Name::new(b"a").is_ok());
        assert!(Name::new(b"...").is_ok());
        assert!(Name::new(b".scionx").is_ok());
        assert!(Name::new(&[0xff, 0xfe]).is_ok());
        assert!(Name::new(&[b'x'; 255]).is_ok());
        for bad in [
            &b""[..],
            b".",
            b"..",
            b"a/b",
            b"a\0b",
            b".scion",
            b".SCION",
            b".Scion",
            &[b'x'; 256],
        ] {
            assert_eq!(
                Name::new(bad).unwrap_err().subcode(),
                Some(subcode::INVALID_PATH),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn navigation() {
        let x = p("a/b/c");
        assert_eq!(x.parent(), Some(p("a/b")));
        assert_eq!(p("a").parent(), Some(RepoPath::root()));
        assert_eq!(RepoPath::root().parent(), None);
        assert_eq!(x.file_name(), Some(&b"c"[..]));
        assert_eq!(RepoPath::root().file_name(), None);
        assert_eq!(x.components().collect::<Vec<_>>(), vec![b"a", b"b", b"c"]);
        assert_eq!(RepoPath::root().components().count(), 0);
        assert_eq!(x.ancestors(), vec![RepoPath::root(), p("a"), p("a/b")]);
        assert!(RepoPath::root().ancestors().is_empty());
        assert_eq!(x.depth(), 3);
        assert_eq!(RepoPath::root().depth(), 0);
        assert!(p("a").is_ancestor_of(&x));
        assert!(p("a/b").is_ancestor_of(&x));
        assert!(RepoPath::root().is_ancestor_of(&x));
        assert!(!x.is_ancestor_of(&x));
        assert!(!p("a").is_ancestor_of(&p("ab")));
        assert!(!p("a/b").is_ancestor_of(&p("a/bc/d")));
        assert!(!RepoPath::root().is_ancestor_of(&RepoPath::root()));
        let n = Name::new(b"d").unwrap();
        assert_eq!(x.join(&n).unwrap(), p("a/b/c/d"));
        assert_eq!(RepoPath::root().join(&n).unwrap(), p("d"));
        assert_eq!(x.name(), Some(Name::new(b"c").unwrap()));
    }

    #[test]
    fn join_length_limit() {
        let n = Name::new(&[b'x'; 255]).unwrap();
        let mut path = RepoPath::root();
        // 16 * 255 + 15 = 4095 bytes.
        for _ in 0..16 {
            path = path.join(&n).unwrap();
        }
        assert_eq!(path.as_bytes().len(), 4095);
        assert!(path.join(&Name::new(b"y").unwrap()).is_err());
    }
}
