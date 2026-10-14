//! Shared helpers of the scion-import integration tests (P29).
#![allow(dead_code)]

use scion_core::{ErrorCode, ObjectId, ScionError};
use scion_import::testing::{
    IMPORTER_SECRET, IMPORTER_SERVICE, ScionFile, ScionReader, importer_acl, importer_principal,
};
use scion_proto::convert::{id_bytes, parse_id};
use scion_proto::{ScionClient, error_from_status, v1};
use scion_server::testing::{InProcessServer, ServerTestOpts};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

/// A server whose policy lets `service:importer` import (write on `/`, admin.refs, force).
pub async fn server_with(tls: bool) -> InProcessServer {
    let mut opts = ServerTestOpts {
        acl_toml: importer_acl(),
        service_principals: vec![(IMPORTER_SERVICE.to_owned(), IMPORTER_SECRET.to_owned())],
        tls,
        ..Default::default()
    };
    // The differential tests issue thousands of small RPCs; lift the per-principal rate limit
    // so they do not spend their time in back-off (the importer itself retries
    // RESOURCE_EXHAUSTED; see `import_rate_limited_server_still_imports`).
    opts.tweak = tweak_of(&opts.tweak, |c| {
        c.rate_limit.per_principal_rps = 1_000_000;
        c.rate_limit.burst = 1_000_000;
    });
    InProcessServer::start(opts).await
}

/// A server with the default (500 rps, burst 1000) or a custom rate limit.
pub async fn server_with_rate(rps: u32, burst: u32) -> InProcessServer {
    let mut opts = ServerTestOpts {
        acl_toml: importer_acl(),
        service_principals: vec![(IMPORTER_SERVICE.to_owned(), IMPORTER_SECRET.to_owned())],
        ..Default::default()
    };
    opts.tweak = tweak_of(&opts.tweak, move |c| {
        c.rate_limit.per_principal_rps = rps;
        c.rate_limit.burst = burst;
    });
    InProcessServer::start(opts).await
}

/// Box a config tweak; the config type is inferred from the `ServerTestOpts::tweak` field
/// passed as a witness (this crate does not depend on scion-config).
pub fn tweak_of<T>(
    _witness: &Option<std::sync::Arc<dyn Fn(&mut T) + Send + Sync>>,
    f: impl Fn(&mut T) + Send + Sync + 'static,
) -> Option<std::sync::Arc<dyn Fn(&mut T) + Send + Sync>> {
    Some(std::sync::Arc::new(f))
}

/// [`server_with`] over TLS.
pub async fn server() -> InProcessServer {
    server_with(true).await
}

/// A client authenticated as `service:importer`.
pub async fn importer_client(s: &InProcessServer) -> ScionClient {
    s.client(Some(&importer_principal())).await
}

/// The vendored bundle `corpus/git/<name>.bundle`.
pub fn bundle(name: &str) -> PathBuf {
    scion_testkit::git::bundles()
        .into_iter()
        .find(|p| p.file_stem().is_some_and(|s| s == name))
        .unwrap_or_else(|| panic!("bundle {name} not found"))
}

/// Unpack bundle `name` into `dir` (gix only); returns the bare repository path.
pub fn unpack(name: &str, dir: &Path) -> PathBuf {
    scion_testkit::git::unpack(&bundle(name), dir).expect("unpack bundle")
}

/// Every ref of a bundle (`refs/...`, header order; `HEAD` excluded).
pub fn bundle_refs(name: &str) -> Vec<String> {
    scion_testkit::git::bundle_header(&bundle(name))
        .expect("bundle header")
        .refs
        .into_iter()
        .map(|(n, _)| n)
        .filter(|n| n.starts_with("refs/"))
        .collect()
}

/// A Scion ref name for importing git ref `r`: `imp/<r without refs/>`, sanitized.
pub fn target_for(r: &str) -> String {
    let s: String = r
        .strip_prefix("refs/")
        .unwrap_or(r)
        .chars()
        .map(|c| {
            let c = c.to_ascii_lowercase();
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '/' | '-') {
                c
            } else {
                '-'
            }
        })
        .collect();
    format!("imp/{}", s.replace("..", "-"))
}

/// `ResolveRef(name)`: `(target, version)` or the error.
pub async fn resolve_ref(c: &ScionClient, name: &str) -> Result<(ObjectId, u64), ScionError> {
    let r = c
        .objects()
        .resolve_ref(v1::ResolveRefRequest {
            name: name.to_owned(),
            at_version: None,
        })
        .await
        .map_err(|s| error_from_status(&s))?
        .into_inner();
    Ok((parse_id(&r.target, "target").unwrap(), r.version))
}

/// Imported changeset of git commit `sha` (panics when not imported).
pub async fn mapped(c: &ScionClient, sha: &str) -> ObjectId {
    scion_import::testing::resolve_git(c, sha)
        .await
        .unwrap()
        .unwrap_or_else(|| panic!("git:{sha} not imported"))
}

/// The files of changeset `cs` with their contents.
pub async fn scion_files_with_content(
    c: &ScionClient,
    cs: &ObjectId,
) -> BTreeMap<Vec<u8>, (ScionFile, Vec<u8>)> {
    let mut r = ScionReader::new(c);
    let changeset = r.changeset(cs).await.unwrap();
    let files = r.files(&changeset.root_tree).await.unwrap();
    let mut out = BTreeMap::new();
    for (p, f) in files {
        let b = r.blob(&f.id).await.unwrap();
        out.insert(p, (f, b.to_vec()));
    }
    out
}

/// True when the error has `code`.
pub fn is_code<T: std::fmt::Debug>(r: &Result<T, ScionError>, code: ErrorCode) -> bool {
    matches!(r, Err(e) if e.code == code)
}

/// A git CLI command in `repo` with user/system configuration disabled.
pub fn git(repo: &Path) -> Command {
    let mut cmd = Command::new("git");
    cmd.arg("--git-dir")
        .arg(repo)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("HOME", repo)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE");
    cmd
}

/// Run a git command and return stdout (panics on failure).
pub fn git_out(repo: &Path, args: &[&str]) -> Vec<u8> {
    let out = git(repo).args(args).output().expect("run git");
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    out.stdout
}

/// `GetChangesets` id helper.
pub fn idb(id: &ObjectId) -> Vec<u8> {
    id_bytes(id)
}
