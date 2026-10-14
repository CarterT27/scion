//! Shared helpers for the P11 `local_*` integration tests.
#![allow(dead_code, clippy::unwrap_used)] // test helpers

use scion_object::RepoPath;

/// True when `RepoPath::parse` is implemented (P01). Tests that need non-root `RepoPath`
/// values skip (with a `SKIP:` line) on a tree where P01 is still a stub.
pub fn p01_ready() -> bool {
    std::panic::catch_unwind(|| RepoPath::parse(b"a/b").is_ok()).unwrap_or(false)
}

/// Return early from a test when P01 is not available.
#[macro_export]
macro_rules! require_p01 {
    () => {
        if !$crate::local_support::p01_ready() {
            eprintln!("SKIP: RepoPath::parse (P01) is not implemented in this tree");
            return;
        }
    };
}

/// Parse a repository path.
pub fn rp(s: &str) -> RepoPath {
    RepoPath::parse(s.as_bytes()).unwrap()
}

/// Parse a repository path from bytes.
pub fn rpb(s: &[u8]) -> RepoPath {
    RepoPath::parse(s).unwrap()
}

/// True when the `git` CLI is available.
pub fn have_git() -> bool {
    std::process::Command::new("git")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}
