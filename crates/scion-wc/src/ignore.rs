//! gitignore semantics (SPEC 11.4; P11).
//!
//! Every `.gitignore` below the root is compiled with `ignore::gitignore`, rooted at its own
//! directory. A path is matched git-style:
//!
//! - files are consulted from the deepest directory upwards; within a file the last matching
//!   pattern wins, and the first file with a match decides (negation `!` re-includes);
//! - a path below an ignored directory is ignored, whatever deeper patterns say (git does not
//!   descend into excluded directories, so a file cannot be re-included under them);
//! - `.gitignore` files inside ignored directories are not read, and symlinked `.gitignore`
//!   files or directories are never followed.
//!
//! Matching is bytewise case-sensitive (SPEC 4.4 paths compare bytewise).

use crate::fs_safe::io_error;
use ::ignore::Match;
use ::ignore::gitignore::{Gitignore, GitignoreBuilder};
use scion_core::Result;
use scion_object::RepoPath;
use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

/// File name of ignore files.
const GITIGNORE: &str = ".gitignore";

/// Nested `.gitignore` matcher with git semantics (SPEC 11.4).
pub struct IgnoreMatcher {
    /// Compiled matcher per directory (relative path bytes; `""` is the root).
    matchers: BTreeMap<Vec<u8>, Gitignore>,
}

impl std::fmt::Debug for IgnoreMatcher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IgnoreMatcher")
            .field("files", &self.matchers.len())
            .finish()
    }
}

impl IgnoreMatcher {
    /// Load every `.gitignore` under `root` (SPEC 11.4), skipping `.scion` and directories
    /// already ignored by their ancestors' files.
    pub fn load(root: &Path) -> Result<Self> {
        let mut m = IgnoreMatcher {
            matchers: BTreeMap::new(),
        };
        let mut stack: Vec<Vec<u8>> = vec![Vec::new()];
        while let Some(rel) = stack.pop() {
            let dir = if rel.is_empty() {
                root.to_path_buf()
            } else {
                root.join(OsStr::from_bytes(&rel))
            };
            let entries = match std::fs::read_dir(&dir) {
                Ok(e) => e,
                Err(e) if rel.is_empty() => return Err(io_error("read working-copy root", e)),
                Err(e) => {
                    tracing::debug!(error = %e, "skipping unreadable directory while loading ignore files");
                    continue;
                }
            };
            let mut subdirs = Vec::new();
            let mut has_ignore_file = false;
            for entry in entries {
                let Ok(entry) = entry else { continue };
                let Ok(ft) = entry.file_type() else { continue };
                let name = entry.file_name();
                let name = name.as_bytes();
                if ft.is_dir() {
                    if name.eq_ignore_ascii_case(b".scion") {
                        continue;
                    }
                    subdirs.push(join(&rel, name));
                } else if ft.is_file() && name == GITIGNORE.as_bytes() {
                    has_ignore_file = true;
                }
            }
            if has_ignore_file {
                let text = std::fs::read(dir.join(GITIGNORE))
                    .map_err(|e| io_error("read .gitignore", e))?;
                m.matchers.insert(rel.clone(), compile(&dir, &text));
            }
            subdirs.sort();
            for sub in subdirs.into_iter().rev() {
                if !m.is_ignored_bytes(&sub, true) {
                    stack.push(sub);
                }
            }
        }
        Ok(m)
    }

    /// A matcher built from in-memory `.gitignore` contents, keyed by directory (`""` for the
    /// root). Used by tests and callers that do not read the filesystem (SPEC 11.4).
    pub fn from_files<'a>(files: impl IntoIterator<Item = (&'a [u8], &'a [u8])>) -> Self {
        let mut matchers = BTreeMap::new();
        for (dir, text) in files {
            let base = Path::new("/").join(OsStr::from_bytes(dir));
            matchers.insert(dir.to_vec(), compile(&base, text));
        }
        IgnoreMatcher { matchers }
    }

    /// True when `p` is ignored (SPEC 11.4): it matches an ignore pattern (not re-included by
    /// a later negation), or one of its parent directories is ignored.
    pub fn is_ignored(&self, p: &RepoPath, is_dir: bool) -> bool {
        self.is_ignored_bytes(p.as_bytes(), is_dir)
    }

    /// Byte-level [`IgnoreMatcher::is_ignored`].
    pub fn is_ignored_bytes(&self, p: &[u8], is_dir: bool) -> bool {
        if p.is_empty() || self.matchers.is_empty() {
            return false;
        }
        for (k, b) in p.iter().enumerate() {
            if *b == b'/' && self.match_one(&p[..k], true) {
                return true;
            }
        }
        self.match_one(p, is_dir)
    }

    /// Decide one path against the `.gitignore` files of its ancestors, deepest first.
    fn match_one(&self, p: &[u8], is_dir: bool) -> bool {
        let mut cuts: Vec<usize> = vec![0];
        cuts.extend(
            p.iter()
                .enumerate()
                .filter(|(_, b)| **b == b'/')
                .map(|(k, _)| k),
        );
        for &cut in cuts.iter().rev() {
            let dir = &p[..cut];
            let Some(gi) = self.matchers.get(dir) else {
                continue;
            };
            let rel = if cut == 0 { p } else { &p[cut + 1..] };
            match gi.matched(Path::new(OsStr::from_bytes(rel)), is_dir) {
                Match::Ignore(_) => return true,
                Match::Whitelist(_) => return false,
                Match::None => {}
            }
        }
        false
    }
}

fn join(dir: &[u8], name: &[u8]) -> Vec<u8> {
    if dir.is_empty() {
        name.to_vec()
    } else {
        let mut v = Vec::with_capacity(dir.len() + 1 + name.len());
        v.extend_from_slice(dir);
        v.push(b'/');
        v.extend_from_slice(name);
        v
    }
}

/// Compile one `.gitignore`; invalid patterns are skipped (git ignores them too).
fn compile(dir: &Path, text: &[u8]) -> Gitignore {
    let mut b = GitignoreBuilder::new(dir);
    b.case_insensitive(false).ok();
    for line in text.split(|c| *c == b'\n') {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        let line = String::from_utf8_lossy(line);
        if let Err(e) = b.add_line(None, &line) {
            tracing::debug!(error = %e, "skipping invalid .gitignore pattern");
        }
    }
    match b.build() {
        Ok(gi) => gi,
        Err(e) => {
            tracing::warn!(error = %e, "invalid .gitignore; ignoring its patterns");
            Gitignore::empty()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nested_negation_and_parents() {
        let m = IgnoreMatcher::from_files([
            (
                &b""[..],
                &b"*.log\n!keep.log\nbuild/\n/top\ndocs/*.tmp\n"[..],
            ),
            (b"sub", b"!*.log\nlocal\n"),
        ]);
        assert!(m.is_ignored_bytes(b"a.log", false));
        assert!(m.is_ignored_bytes(b"x/y/a.log", false));
        assert!(!m.is_ignored_bytes(b"keep.log", false));
        assert!(
            !m.is_ignored_bytes(b"sub/a.log", false),
            "deeper file re-includes"
        );
        assert!(m.is_ignored_bytes(b"sub/local", false));
        assert!(!m.is_ignored_bytes(b"local", false));
        assert!(m.is_ignored_bytes(b"build", true));
        assert!(!m.is_ignored_bytes(b"build", false), "dir-only pattern");
        assert!(
            m.is_ignored_bytes(b"build/x/keep.log", false),
            "under an ignored dir"
        );
        assert!(m.is_ignored_bytes(b"top", false));
        assert!(!m.is_ignored_bytes(b"a/top", false), "anchored");
        assert!(m.is_ignored_bytes(b"docs/a.tmp", false));
        assert!(!m.is_ignored_bytes(b"docs/x/a.tmp", false));
        assert!(!m.is_ignored_bytes(b"", true));
        assert!(!m.is_ignored_bytes(b"A.LOG", false), "case-sensitive");
    }

    #[test]
    fn load_from_disk_skips_ignored_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let r = dir.path();
        std::fs::write(r.join(".gitignore"), "out/\n*.o\n").unwrap();
        std::fs::create_dir_all(r.join("out/deep")).unwrap();
        std::fs::write(r.join("out/.gitignore"), "!*.o\n").unwrap();
        std::fs::create_dir_all(r.join("src")).unwrap();
        std::fs::write(r.join("src/.gitignore"), "gen.rs\r\n").unwrap();
        std::fs::create_dir_all(r.join(".scion")).unwrap();
        std::fs::write(r.join(".scion/.gitignore"), "*\n").unwrap();
        let m = IgnoreMatcher::load(r).unwrap();
        assert_eq!(m.matchers.len(), 2, "root and src only");
        assert!(m.is_ignored_bytes(b"a.o", false));
        assert!(m.is_ignored_bytes(b"out/a.o", false));
        assert!(m.is_ignored_bytes(b"src/gen.rs", false));
        assert!(!m.is_ignored_bytes(b"gen.rs", false));
        assert!(!m.is_ignored_bytes(b"src/main.rs", false));
    }
}
