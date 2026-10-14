//! Shared harness of the scion-cli integration tests (P31): a [`TestCluster`], a fake `$HOME`,
//! the CA file of the test server, an in-process [`scion_cli::run`] driver, the published
//! schemas and the scripted end-to-end session used by the `--json` and smoke suites.
#![allow(dead_code, clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use scion_cli::CliIo;
use scion_config::MapEnv;
use scion_conformance::{ClusterOpts, TestCluster, Via};
use scion_core::{ObjectId, Principal};
use scion_proto::{Endpoint, v1};
use serde_json::Value;
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::io::Write;
use std::path::{Path, PathBuf};

/// A cluster plus client-side fixtures.
pub struct Harness {
    pub cluster: TestCluster,
    pub home: tempfile::TempDir,
    pub work: tempfile::TempDir,
    pub ca_file: PathBuf,
    pub endpoint: String,
}

/// The text form of an endpoint (`unix:///...` or `https://...`).
pub fn endpoint_text(ep: &Endpoint) -> String {
    match ep {
        Endpoint::Unix(p) => format!("unix://{}", p.display()),
        Endpoint::Https(u) => u.clone(),
    }
}

impl Harness {
    /// One server (TLS), optionally a daemon.
    pub async fn start(daemon: bool) -> Self {
        let cluster = TestCluster::start(ClusterOpts::default().with_daemon(daemon)).await;
        let home = tempfile::tempdir().unwrap();
        let work = tempfile::tempdir().unwrap();
        let ca_file = home.path().join("ca.pem");
        let tls = cluster.servers[0]
            .tls
            .clone()
            .expect("test server uses TLS");
        std::fs::write(&ca_file, tls.ca_pem.expect("CA pem")).unwrap();
        let endpoint = endpoint_text(&cluster.servers[0].endpoint);
        Harness {
            cluster,
            home,
            work,
            ca_file,
            endpoint,
        }
    }

    /// The daemon's endpoint text.
    pub fn daemon_endpoint(&self) -> String {
        endpoint_text(&self.cluster.daemon.as_ref().unwrap().handle.endpoint())
    }

    /// `HOME`, `SCION_ENDPOINT` and `SCION_CA_FILE`, no token.
    pub fn env_anon(&self) -> MapEnv {
        MapEnv::from_pairs([
            ("HOME", self.home.path().to_str().unwrap()),
            ("SCION_ENDPOINT", self.endpoint.as_str()),
            ("SCION_CA_FILE", self.ca_file.to_str().unwrap()),
        ])
    }

    /// [`Harness::env_anon`] plus `SCION_TOKEN` for `github:<login>`.
    pub fn env_as(&self, login: &str) -> MapEnv {
        let mut env = self.env_anon();
        env.0.insert(
            "SCION_TOKEN".into(),
            self.cluster.token_for(&TestCluster::gh(login)),
        );
        env
    }

    /// A path under the scratch directory.
    pub fn path(&self, rel: &str) -> PathBuf {
        self.work.path().join(rel)
    }

    /// Current target of ref `main`.
    pub async fn main_head(&self) -> ObjectId {
        self.cluster.ref_head("main").await.0
    }

    /// Create a changeset putting `puts` on the current `main` and land it, as `who`
    /// (a competing change from another client).
    pub async fn land_change(&self, who: &Principal, puts: &[(&str, &[u8])], n: u32) -> ObjectId {
        let client = self.cluster.client(Via::Server, who).await;
        let head = self.main_head().await;
        let cs = TestCluster::commit(&client, &head, puts, &[], n).await;
        let r = client
            .write()
            .land(v1::LandRequest {
                r#ref: "main".into(),
                head: cs.as_bytes().to_vec(),
                allow_empty: false,
            })
            .await
            .unwrap()
            .into_inner();
        ObjectId::from_bytes(&r.target).unwrap()
    }
}

/// Captured process output.
#[derive(Clone, Debug)]
pub struct Out {
    pub code: i32,
    pub stdout: Vec<u8>,
    pub stderr: String,
}

impl Out {
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.stdout).into_owned()
    }

    /// The single JSON line on stdout.
    pub fn json(&self) -> Value {
        let t = self.text();
        let lines: Vec<&str> = t.lines().collect();
        assert_eq!(
            lines.len(),
            1,
            "expected one JSON line, got {t:?} (stderr {})",
            self.stderr
        );
        serde_json::from_str(lines[0]).unwrap_or_else(|e| panic!("bad JSON {t:?}: {e}"))
    }
}

struct BufIo {
    out: Vec<u8>,
    err: Vec<u8>,
    stdin: Vec<String>,
}

impl CliIo for BufIo {
    fn stdout(&mut self) -> &mut dyn Write {
        &mut self.out
    }
    fn stderr(&mut self) -> &mut dyn Write {
        &mut self.err
    }
    fn stdin_line(&mut self) -> Option<String> {
        if self.stdin.is_empty() {
            None
        } else {
            Some(self.stdin.remove(0))
        }
    }
}

/// Run `scion <args>` in-process on a blocking thread (it builds its own runtime).
pub async fn scion(env: &MapEnv, args: &[&str]) -> Out {
    scion_stdin(env, args, &[]).await
}

/// [`scion`] with standard-input lines.
pub async fn scion_stdin(env: &MapEnv, args: &[&str], stdin: &[&str]) -> Out {
    let env = env.clone();
    let mut argv: Vec<OsString> = vec!["scion".into()];
    argv.extend(args.iter().map(OsString::from));
    let stdin: Vec<String> = stdin.iter().map(|s| (*s).to_owned()).collect();
    tokio::task::spawn_blocking(move || {
        let mut io = BufIo {
            out: Vec::new(),
            err: Vec::new(),
            stdin,
        };
        let code = scion_cli::run(argv, &env, &mut io);
        Out {
            code,
            stdout: io.out,
            stderr: String::from_utf8_lossy(&io.err).into_owned(),
        }
    })
    .await
    .unwrap()
}

/// `<repo>/schemas/v1`.
pub fn schemas_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../schemas/v1")
}

/// The committed schema of `cmd`.
pub fn published_schema(cmd: &str) -> Value {
    let p = schemas_dir().join(format!("{cmd}.schema.json"));
    let text = std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display()));
    serde_json::from_str(&text).unwrap()
}

/// Validate an envelope against the published schema its `schema` field names; returns the
/// command name.
pub fn validate(v: &Value) -> String {
    let id = v["schema"]
        .as_str()
        .unwrap_or_else(|| panic!("no schema field: {v}"));
    let cmd = id
        .strip_prefix("scion.cli.")
        .and_then(|s| s.strip_suffix(".v1"))
        .unwrap_or_else(|| panic!("bad schema id {id}"));
    let schema = published_schema(cmd);
    if let Err(e) = jsonschema::validate(&schema, v) {
        panic!("{cmd} output does not match schemas/v1/{cmd}.schema.json: {e}\n{v:#}");
    }
    cmd.to_owned()
}

/// One step of the scripted session.
#[derive(Debug)]
pub struct Step {
    pub label: &'static str,
    pub cmd: &'static str,
    pub out: Out,
}

fn put(path: &Path, content: &[u8]) {
    if let Some(p) = path.parent() {
        std::fs::create_dir_all(p).unwrap();
    }
    std::fs::write(path, content).unwrap();
}

/// The scripted end-to-end session over every command (SPEC 11.8), in human or `--json`
/// mode. Exit codes are asserted here; callers check the output.
pub async fn scripted_session(h: &Harness, json: bool) -> Vec<Step> {
    let alice = h.env_as("alice");
    let admin = h.env_as("admin");
    let wc1 = h.path("wc1");
    let wc2 = h.path("wc2");
    let wc1s = wc1.to_str().unwrap().to_owned();
    let wc2s = wc2.to_str().unwrap().to_owned();
    let mut steps = Vec::new();
    macro_rules! step {
        ($label:expr, $cmd:expr, $env:expr, $code:expr, [$($a:expr),* $(,)?]) => {{
            let mut args: Vec<&str> = Vec::new();
            if json {
                args.push("--json");
            }
            $(args.push($a);)*
            let out = scion($env, &args).await;
            assert_eq!(
                out.code, $code,
                "step {}: scion {:?} exited {} (stdout {:?}, stderr {:?})",
                $label, args, out.code, out.text(), out.stderr
            );
            steps.push(Step { label: $label, cmd: $cmd, out: out.clone() });
            out
        }};
    }

    // Service login (credentials written under the fake HOME).
    let mut svc = h.env_anon();
    svc.0.insert(
        "SCION_SERVICE_SECRET".into(),
        TestCluster::SERVICES[0].1.into(),
    );
    step!("login", "login", &svc, 0, ["login", "--service", "ci"]);
    step!("whoami", "whoami", &alice, 0, ["whoami"]);
    step!(
        "checkout",
        "checkout",
        &alice,
        0,
        ["checkout", "main", &wc1s]
    );
    put(&wc1.join("a.txt"), b"one\ntwo\n");
    put(&wc1.join("dir/b.txt"), b"b\n");
    step!("status-dirty", "status", &alice, 0, ["-C", &wc1s, "status"]);
    step!("diff", "diff", &alice, 0, ["-C", &wc1s, "diff"]);
    step!(
        "commit",
        "commit",
        &alice,
        0,
        ["-C", &wc1s, "commit", "-m", "add files"]
    );
    step!(
        "amend",
        "amend",
        &alice,
        0,
        ["-C", &wc1s, "amend", "-m", "add files (amended)"]
    );
    step!("land", "land", &alice, 0, ["-C", &wc1s, "land"]);
    step!(
        "log",
        "log",
        &alice,
        0,
        ["-C", &wc1s, "log", "--limit", "5"]
    );
    step!(
        "log-path",
        "log",
        &alice,
        0,
        ["-C", &wc1s, "log", "a.txt", "--oneline"]
    );
    step!("show", "show", &alice, 0, ["-C", &wc1s, "show"]);
    step!("cat", "cat", &alice, 0, ["cat", "main:a.txt"]);
    step!(
        "blame",
        "blame",
        &alice,
        0,
        ["blame", "a.txt", "--rev", "main"]
    );
    step!(
        "checkout-sparse",
        "checkout",
        &alice,
        0,
        ["checkout", "main", &wc2s, "--sparse", "dir"]
    );
    assert!(!wc2.join("a.txt").exists());
    step!(
        "sparse-add",
        "sparse",
        &alice,
        0,
        ["-C", &wc2s, "sparse", "add", "a.txt"]
    );
    assert_eq!(std::fs::read(wc2.join("a.txt")).unwrap(), b"one\ntwo\n");
    step!(
        "sparse-remove",
        "sparse",
        &alice,
        0,
        ["-C", &wc2s, "sparse", "remove", "a.txt"]
    );
    assert!(!wc2.join("a.txt").exists());
    step!(
        "goto-back",
        "goto",
        &alice,
        0,
        ["-C", &wc1s, "goto", "main~1"]
    );
    assert!(!wc1.join("a.txt").exists());
    step!(
        "goto-main",
        "goto",
        &alice,
        0,
        ["-C", &wc1s, "goto", "main"]
    );
    // Dirty refusal: the move would delete the modified a.txt.
    put(&wc1.join("a.txt"), b"dirty\n");
    step!(
        "goto-dirty",
        "error",
        &alice,
        4,
        ["-C", &wc1s, "goto", "main~1"]
    );
    put(&wc1.join("a.txt"), b"one\ntwo\n");
    // A competing change lands on main; the local change conflicts with it.
    h.land_change(
        &TestCluster::gh("bob"),
        &[("a.txt", b"one\nTHEIRS\n")],
        9001,
    )
    .await;
    put(&wc1.join("a.txt"), b"one\nOURS\n");
    step!(
        "commit-ours",
        "commit",
        &alice,
        0,
        ["-C", &wc1s, "commit", "-m", "ours"]
    );
    step!(
        "rebase-conflict",
        "rebase",
        &alice,
        3,
        ["-C", &wc1s, "rebase", "--onto", "main"]
    );
    step!(
        "status-conflict",
        "status",
        &alice,
        0,
        ["-C", &wc1s, "status"]
    );
    step!(
        "rebase-abort",
        "rebase",
        &alice,
        0,
        ["-C", &wc1s, "rebase", "--abort"]
    );
    step!(
        "rebase-again",
        "rebase",
        &alice,
        3,
        ["-C", &wc1s, "rebase", "--onto", "main"]
    );
    step!(
        "resolve-markers",
        "error",
        &alice,
        3,
        ["-C", &wc1s, "resolve", "a.txt"]
    );
    put(&wc1.join("a.txt"), b"one\nMERGED\n");
    step!(
        "resolve",
        "resolve",
        &alice,
        0,
        ["-C", &wc1s, "resolve", "a.txt"]
    );
    step!(
        "commit-continue",
        "commit",
        &alice,
        0,
        ["-C", &wc1s, "commit"]
    );
    step!("land-merged", "land", &alice, 0, ["-C", &wc1s, "land"]);
    // Outside a rebase a message is required: usage error.
    step!(
        "commit-no-message",
        "error",
        &alice,
        2,
        ["-C", &wc1s, "commit"]
    );
    let blob = ObjectId::hash_blob(b"b\n").to_text();
    step!(
        "redact",
        "redact",
        &admin,
        0,
        ["redact", "--object", &blob, "--reason", "smoke"]
    );
    step!(
        "cat-missing",
        "error",
        &alice,
        1,
        ["cat", "main:missing.txt"]
    );
    steps
}

/// Steps by label.
pub fn by_label(steps: &[Step]) -> BTreeMap<&'static str, &Step> {
    steps.iter().map(|s| (s.label, s)).collect()
}
