//! Whole-system differential against the reference model (PLAN 3.4 item 5; SPEC 19.0, 8.1-8.5,
//! 19.4; P32).
//!
//! A seeded random operation driver applies the same operation sequence to the real stack
//! (through RPCs, spread over a [`TestCluster`] of three servers sharing one storage) and to
//! `scion-refmodel` (structural trees, its own merge engine and its own canonical encoder):
//!
//! - **create**: `CreateChangeset` with a random valid delta (edits, adds, deletes, mode
//!   changes, file/directory transitions), or one with a single defect (`delete_absent`,
//!   `noop_put`, `file_directory`, a redacted put);
//! - **rebase**: `Rebase(C, X)` of a random changeset onto a random changeset;
//! - **merge**: `Merge(a, b)`;
//! - **land**: `Land(ref, head)` (with or without `allow_empty`) onto one of two refs;
//! - **update_ref**: version-checked, fast-forward or forced `UpdateRef`;
//! - **redact**: `Redact` of a blob of the history (the model then treats that content as
//!   redacted in creates and merges).
//!
//! The model computes every changeset id with its own encoder, so a successful operation is
//! compared by id (which covers the parents, the result tree, the delta, the Meta and the
//! change id); failures are compared by error code, subcode and the conflict path/kind set;
//! lands by target, version and landed entries. At the end of every seed the refs
//! (`ResolveRef`), their ref logs (`GetRefLog`: order, targets and landed sets) and the
//! flattened trees of the ref targets (`ListTree`) are compared. There must be zero
//! divergences over at least 200 seeds (more under `full_*`).
//!
//! The file also holds SPEC 19.4 rebase determinism: the same `(C, X)` yields the same id on
//! every server process, after restarts, and on an independent cluster.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use futures::StreamExt;
use rand::{Rng, SeedableRng};
use rand_chacha::ChaCha8Rng;
use scion_conformance::{Backend, ClusterOpts, TestCluster, invariants};
use scion_core::{ErrorPayload, ObjectId, Principal};
use scion_proto::convert::{DELTA_OP_DELETE, DELTA_OP_PUT};
use scion_proto::{ScionClient, error_from_status, v1};
use scion_refmodel::enc::{blob_id, changeset_for, changeset_id, meta_id};
use scion_refmodel::{MDelta, MFile, MMeta, MRepo, MTree, apply_delta, file_diff, merge_trees};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Mutex;

/// Servers sharing the storage.
const SERVERS: usize = 3;
/// `limits.merge_max_text_bytes` default (SPEC 6.5).
const MAX_TEXT: u64 = 10_485_760;
/// Seeds run concurrently on one cluster.
const PARALLEL: usize = 8;
/// The refs every seed lands on and updates (`d/<seed>/<name>`).
const REFS: [&str; 2] = ["a", "b"];
/// File paths the driver uses (`x` vs `x/...` gives file/directory transitions).
const PATHS: [&str; 10] = [
    "a.txt",
    "b.txt",
    "d/c.txt",
    "d/e.txt",
    "d/f/g.txt",
    "x",
    "x/y.txt",
    "x/z/w.txt",
    "h.md",
    "d/f",
];
const AUTHOR: &[u8] = b"Differential <differential@example.com>";

type Id = [u8; 34];

fn idb(id: &Id) -> Vec<u8> {
    id.to_vec()
}

fn raw(b: &[u8]) -> Id {
    b.try_into().unwrap_or([0; 34])
}

fn text(id: &Id) -> String {
    scion_refmodel::enc::id_text(id)
}

fn alice() -> Principal {
    TestCluster::gh("alice")
}

fn admin() -> Principal {
    TestCluster::gh("admin")
}

fn skip_external() -> bool {
    if Backend::external_available() {
        return false;
    }
    println!("SKIP: SCION_TEST_CRDB_URL / SCION_TEST_S3_ENDPOINT unset");
    true
}

fn skip_full() -> bool {
    if std::env::var_os("SCION_FULL").is_some() {
        return false;
    }
    println!("SKIP: SCION_FULL unset");
    true
}

// ---------------------------------------------------------------------------------------
// The model side.
// ---------------------------------------------------------------------------------------

#[derive(Clone, Debug)]
struct MCommit {
    id: Id,
    parents: Vec<usize>,
    tree: MTree,
    meta: Id,
    change: [u8; 16],
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct MLanded {
    change: [u8; 16],
    source: Id,
    result: Id,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct MLogEntry {
    version: u64,
    old: Option<Id>,
    new: Id,
    landed: Vec<MLanded>,
}

#[derive(Clone, Debug)]
struct MRef {
    version: u64,
    target: usize,
    log: Vec<MLogEntry>,
}

/// An expected failure.
#[derive(Clone, Debug)]
struct MErr {
    code: &'static str,
    subcode: Option<&'static str>,
    conflicts: Option<BTreeSet<(Vec<u8>, String)>>,
}

impl MErr {
    fn code(code: &'static str) -> Self {
        MErr {
            code,
            subcode: None,
            conflicts: None,
        }
    }

    fn invalid(sub: &'static str) -> Self {
        MErr {
            code: "INVALID_ARGUMENT",
            subcode: Some(sub),
            conflicts: None,
        }
    }

    fn conflict(cs: &[scion_refmodel::MConflict]) -> Self {
        MErr {
            code: "MERGE_CONFLICT",
            subcode: None,
            conflicts: Some(
                cs.iter()
                    .map(|c| (c.path.clone(), c.kind.to_owned()))
                    .collect(),
            ),
        }
    }
}

/// A rebase result before it is registered.
enum Rebased {
    Same(usize),
    New(MCommit),
}

/// One seed's reference state.
struct Model {
    repo: MRepo,
    commits: Vec<MCommit>,
    by_id: BTreeMap<Id, usize>,
    refs: BTreeMap<String, MRef>,
    landed: BTreeMap<(String, [u8; 16]), (Id, Id, u64)>,
    redacted: BTreeSet<Vec<u8>>,
}

impl Model {
    fn new(root: Id) -> Self {
        let mut repo = MRepo::new();
        repo.commit(&[], MTree::new());
        Model {
            repo,
            commits: vec![MCommit {
                id: root,
                parents: Vec::new(),
                tree: MTree::new(),
                meta: [0; 34],
                change: [0; 16],
            }],
            by_id: [(root, 0)].into(),
            refs: BTreeMap::new(),
            landed: BTreeMap::new(),
            redacted: BTreeSet::new(),
        }
    }

    /// Register a commit (deduplicated by id; SPEC 8.1 identical creates return one id).
    fn add(&mut self, c: MCommit) -> usize {
        if let Some(&i) = self.by_id.get(&c.id) {
            return i;
        }
        let i = self.repo.commit(&c.parents, c.tree.clone());
        assert_eq!(i, self.commits.len(), "model indices stay aligned");
        self.by_id.insert(c.id, i);
        self.commits.push(c);
        i
    }

    fn register(&mut self, r: Rebased) -> usize {
        match r {
            Rebased::Same(i) => i,
            Rebased::New(c) => self.add(c),
        }
    }

    fn meta(&self, msg: &[u8], time: i64, creator: &Principal) -> Id {
        meta_id(&MMeta {
            author: AUTHOR.to_vec(),
            author_time: (time, 0),
            committer: None,
            committer_time: None,
            creator: creator.id_string().into_bytes(),
            message: msg.to_vec(),
            ext: Vec::new(),
        })
    }

    /// SPEC 8.1 CreateChangeset on one parent (validation order: redacted puts, step 4, then
    /// the 18.2 tree construction).
    fn create(
        &self,
        parent: usize,
        delta: &[MDelta],
        meta: Id,
        change: [u8; 16],
    ) -> Result<MCommit, MErr> {
        for d in delta {
            if let MDelta::Put(_, f) = d
                && self.redacted.contains(&f.content)
            {
                return Err(MErr::code("REDACTED"));
            }
        }
        let t1 = &self.commits[parent].tree;
        let tree = apply_delta(t1, delta).map_err(MErr::invalid)?;
        let cs = changeset_for(vec![self.commits[parent].id], t1, &tree, meta, change);
        Ok(MCommit {
            id: changeset_id(&cs),
            parents: vec![parent],
            tree,
            meta,
            change,
        })
    }

    /// SPEC 8.2 Rebase with the 8.3 rules over the delta paths (redacted contents conflict).
    fn rebase(&self, c: usize, onto: usize) -> Result<Rebased, MErr> {
        let cc = &self.commits[c];
        if cc.parents.len() != 1 {
            return Err(MErr::invalid("multi_parent"));
        }
        let p = cc.parents[0];
        if p == onto {
            return Ok(Rebased::Same(c));
        }
        let b = &self.commits[p].tree;
        let o = &self.commits[onto].tree;
        let t = &cc.tree;
        let paths: BTreeSet<Vec<u8>> = file_diff(b, t)
            .into_iter()
            .map(|d| match d {
                MDelta::Put(p, _) | MDelta::Delete(p) => p,
            })
            .collect();
        let merged = merge_trees(b, o, t, Some(&paths), MAX_TEXT, &self.redacted)
            .map_err(|cs| MErr::conflict(&cs))?;
        if &merged == o {
            return Err(MErr::code("EMPTY_CHANGE"));
        }
        let cs = changeset_for(vec![self.commits[onto].id], o, &merged, cc.meta, cc.change);
        Ok(Rebased::New(MCommit {
            id: changeset_id(&cs),
            parents: vec![onto],
            tree: merged,
            meta: cc.meta,
            change: cc.change,
        }))
    }

    /// SPEC 8.3 `Merge(a, b)` (PLAN 6.14: no common ancestor merges against the empty tree).
    fn merge(&self, a: usize, b: usize, meta: Id, change: [u8; 16]) -> Result<MCommit, MErr> {
        let bases = self.repo.merge_bases(a, b);
        let base = match bases.as_slice() {
            [] => MTree::new(),
            [x] => self.commits[*x].tree.clone(),
            _ => return Err(MErr::code("AMBIGUOUS_MERGE_BASE")),
        };
        let ta = &self.commits[a].tree;
        let tb = &self.commits[b].tree;
        let merged = merge_trees(&base, ta, tb, None, MAX_TEXT, &self.redacted)
            .map_err(|cs| MErr::conflict(&cs))?;
        let cs = changeset_for(
            vec![self.commits[a].id, self.commits[b].id],
            ta,
            &merged,
            meta,
            change,
        );
        Ok(MCommit {
            id: changeset_id(&cs),
            parents: vec![a, b],
            tree: merged,
            meta,
            change,
        })
    }

    /// SPEC 8.4 / 18.4 Land (one attempt, no concurrency): returns `(target, version, landed)`
    /// and applies the effect.
    fn land(
        &mut self,
        name: &str,
        head: usize,
        allow_empty: bool,
    ) -> Result<(Id, u64, Vec<MLanded>), MErr> {
        let r = self.refs[name].clone();
        let tip = r.target;
        // Stack: first parents from head back to (excluding) the first ancestor of tip.
        let mut stack = Vec::new();
        let mut cur = head;
        while !self.repo.is_ancestor(cur, tip) {
            stack.push(cur);
            match self.commits[cur].parents.first() {
                Some(&p) => cur = p,
                None => break,
            }
        }
        stack.reverse();
        if stack.iter().any(|&m| self.commits[m].parents.len() != 1) {
            return Err(MErr::invalid("merge_in_stack"));
        }
        let mut recorded = Vec::new();
        let mut pending = Vec::new();
        for &m in &stack {
            let mc = &self.commits[m];
            match self.landed.get(&(name.to_owned(), mc.change)) {
                Some((src, res, _)) if *src == mc.id => recorded.push(MLanded {
                    change: mc.change,
                    source: *src,
                    result: *res,
                }),
                Some(_) => return Err(MErr::code("ALREADY_LANDED")),
                None => pending.push(m),
            }
        }
        if pending.is_empty() {
            return Ok((self.commits[tip].id, r.version, recorded));
        }
        let changes: BTreeSet<[u8; 16]> = pending.iter().map(|&m| self.commits[m].change).collect();
        if changes.len() != pending.len() {
            return Err(MErr::invalid("duplicate"));
        }
        let mut onto = tip;
        let mut results = Vec::new();
        for &m in &pending {
            let next = match self.rebase(m, onto) {
                Ok(r) => r,
                Err(e) if e.code == "EMPTY_CHANGE" && allow_empty => Rebased::Same(onto),
                Err(e) => return Err(e),
            };
            // Register now: the rebase result is persisted even when a later member fails,
            // but only a successful land makes the model reference it.
            let idx = match next {
                Rebased::Same(i) => i,
                Rebased::New(c) => self.add(c),
            };
            results.push((m, idx));
            onto = idx;
        }
        let version = r.version + 1;
        let rows: Vec<MLanded> = results
            .iter()
            .map(|&(m, res)| MLanded {
                change: self.commits[m].change,
                source: self.commits[m].id,
                result: self.commits[res].id,
            })
            .collect();
        for l in &rows {
            self.landed
                .insert((name.to_owned(), l.change), (l.source, l.result, version));
        }
        let target_id = self.commits[onto].id;
        let entry = MLogEntry {
            version,
            old: Some(self.commits[tip].id),
            new: target_id,
            landed: rows.clone(),
        };
        let rr = self.refs.get_mut(name).expect("ref exists");
        rr.version = version;
        rr.target = onto;
        rr.log.push(entry);
        recorded.extend(rows);
        Ok((target_id, version, recorded))
    }

    /// SPEC 8.5 UpdateRef.
    fn update_ref(
        &mut self,
        name: &str,
        expected: u64,
        target: usize,
        force_allowed: bool,
    ) -> Result<u64, MErr> {
        let r = self.refs[name].clone();
        if r.version != expected {
            return Err(MErr::code("REF_VERSION_MISMATCH"));
        }
        if !self.repo.is_ancestor(r.target, target) && !force_allowed {
            return Err(MErr::code("NOT_FAST_FORWARD"));
        }
        let version = r.version + 1;
        let entry = MLogEntry {
            version,
            old: Some(self.commits[r.target].id),
            new: self.commits[target].id,
            landed: Vec::new(),
        };
        let rr = self.refs.get_mut(name).expect("ref exists");
        rr.version = version;
        rr.target = target;
        rr.log.push(entry);
        Ok(version)
    }

    /// Every blob content in the history.
    fn contents(&self) -> BTreeSet<Vec<u8>> {
        self.commits
            .iter()
            .flat_map(|c| c.tree.values().map(|f| f.content.clone()))
            .collect()
    }
}

// ---------------------------------------------------------------------------------------
// Random operations.
// ---------------------------------------------------------------------------------------

fn is_under(dir: &[u8], p: &[u8]) -> bool {
    p.len() > dir.len() && p.starts_with(dir) && p[dir.len()] == b'/'
}

fn ancestors_of(p: &[u8]) -> Vec<Vec<u8>> {
    p.iter()
        .enumerate()
        .filter(|(_, b)| **b == b'/')
        .map(|(i, _)| p[..i].to_vec())
        .collect()
}

fn lines(c: &[u8]) -> Vec<Vec<u8>> {
    scion_refmodel::diff::split_lines(c)
        .into_iter()
        .map(<[u8]>::to_vec)
        .collect()
}

/// A random edit of `tree` (never a no-op). `tag` makes new contents unique per seed.
fn mutate(tree: &MTree, rng: &mut ChaCha8Rng, tag: &str) -> MTree {
    let mut t = tree.clone();
    let n = rng.random_range(1..=3);
    for k in 0..n {
        let files: Vec<Vec<u8>> = t.keys().cloned().collect();
        let roll = rng.random_range(0..100);
        if roll < 40 && !files.is_empty() {
            // Edit a line region of an existing file.
            let p = files[rng.random_range(0..files.len())].clone();
            let f = t.get_mut(&p).expect("file");
            let mut ls = lines(&f.content);
            let j = rng.random_range(0..ls.len());
            match rng.random_range(0..3) {
                0 => ls[j] = format!("{tag} edit {k} at {j}\n").into_bytes(),
                1 => ls.insert(j, format!("{tag} insert {k} at {j}\n").into_bytes()),
                _ if ls.len() > 1 => {
                    ls.remove(j);
                }
                _ => ls.push(format!("{tag} append {k}\n").into_bytes()),
            }
            if let Some(last) = ls.last_mut()
                && !last.ends_with(b"\n")
            {
                last.push(b'\n');
            }
            f.content = ls.concat();
        } else if roll < 70 {
            // Add (or replace) a file at a pool path, with the required structural deletes.
            let p = PATHS[rng.random_range(0..PATHS.len())].as_bytes().to_vec();
            t.retain(|q, _| !is_under(&p, q));
            for a in ancestors_of(&p) {
                t.remove(&a);
            }
            let count = rng.random_range(3..8);
            let content: Vec<u8> = (0..count)
                .map(|i| format!("{} {i} {tag} {k}\n", String::from_utf8_lossy(&p)))
                .collect::<String>()
                .into_bytes();
            let mode = match rng.random_range(0..20) {
                0 => 3,
                1 | 2 => 2,
                _ => 1,
            };
            t.insert(p, MFile { mode, content });
        } else if roll < 90 && !files.is_empty() {
            let p = &files[rng.random_range(0..files.len())];
            t.remove(p);
        } else if !files.is_empty() {
            let p = &files[rng.random_range(0..files.len())];
            let f = t.get_mut(p).expect("file");
            f.mode = if f.mode == 1 { 2 } else { 1 };
        }
    }
    if &t == tree {
        let p = format!("n/{tag}.txt").into_bytes();
        t.retain(|q, _| !(q.as_slice() == b"n" || is_under(&p, q)));
        t.insert(
            p,
            MFile {
                mode: 1,
                content: format!("{tag} fresh\n").into_bytes(),
            },
        );
    }
    t
}

fn to_proto(delta: &[MDelta]) -> Vec<v1::DeltaEntryMsg> {
    let mut out: Vec<v1::DeltaEntryMsg> = delta
        .iter()
        .map(|d| match d {
            MDelta::Put(p, f) => v1::DeltaEntryMsg {
                path: p.clone(),
                op: DELTA_OP_PUT,
                mode: u32::from(f.mode),
                id: blob_id(&f.content).to_vec(),
                size: f.content.len() as u64,
            },
            MDelta::Delete(p) => v1::DeltaEntryMsg {
                path: p.clone(),
                op: DELTA_OP_DELETE,
                mode: 0,
                id: Vec::new(),
                size: 0,
            },
        })
        .collect();
    out.sort_by(|a, b| a.path.cmp(&b.path));
    out
}

fn meta_fields(msg: &[u8], time: i64) -> v1::MetaFields {
    v1::MetaFields {
        author: AUTHOR.to_vec(),
        author_time: Some(v1::Timestamp {
            seconds: time,
            tz_offset_minutes: 0,
        }),
        committer: None,
        committer_time: None,
        message: msg.to_vec(),
        extensions: Vec::new(),
    }
}

// ---------------------------------------------------------------------------------------
// The real side.
// ---------------------------------------------------------------------------------------

struct Env {
    c: TestCluster,
    alice: Vec<ScionClient>,
    admin: Vec<ScionClient>,
    root: Id,
    stats: Mutex<BTreeMap<String, u64>>,
}

impl Env {
    async fn start(backend: Backend) -> Env {
        let c = TestCluster::start(
            ClusterOpts::new(backend)
                .with_servers(SERVERS)
                .with_daemon(false),
        )
        .await;
        let mut alice_clients = Vec::new();
        let mut admin_clients = Vec::new();
        for s in &c.servers {
            for (who, out) in [(alice(), &mut alice_clients), (admin(), &mut admin_clients)] {
                out.push(
                    ScionClient::connect(&s.endpoint, s.tls.clone(), Some(c.token_for(&who)))
                        .await
                        .expect("connect"),
                );
            }
        }
        let (root, _) = c.ref_head("main").await;
        Env {
            c,
            alice: alice_clients,
            admin: admin_clients,
            root: *root.as_bytes(),
            stats: Mutex::new(BTreeMap::new()),
        }
    }

    fn count(&self, what: &str) {
        *self
            .stats
            .lock()
            .unwrap()
            .entry(what.to_owned())
            .or_default() += 1;
    }

    /// Wait until every server's redaction set reached `epoch` (SPEC 13.3 propagation).
    async fn await_redaction_epoch(&self, epoch: u64) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        while self
            .c
            .servers
            .iter()
            .any(|s| s.ctx().redactions.epoch() < epoch)
        {
            assert!(
                std::time::Instant::now() < deadline,
                "redaction epoch did not propagate"
            );
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }
}

/// The comparable parts of a failed call.
fn real_err(s: &tonic::Status) -> (String, Option<String>, Option<BTreeSet<(Vec<u8>, String)>>) {
    let code = TestCluster::error_detail(s)
        .map(|d| d.code)
        .unwrap_or_else(|| format!("grpc {:?}: {}", s.code(), s.message()));
    let e = error_from_status(s);
    let sub = e.subcode().map(str::to_owned);
    let conflicts = match &e.payload {
        ErrorPayload::MergeConflict(r) => Some(
            r.conflicts
                .iter()
                .map(|c| (c.path.clone(), c.kind.as_str().to_owned()))
                .collect(),
        ),
        _ => None,
    };
    (code, sub, conflicts)
}

fn show_conflicts(c: &Option<BTreeSet<(Vec<u8>, String)>>) -> String {
    match c {
        None => "-".into(),
        Some(set) => set
            .iter()
            .map(|(p, k)| format!("{}:{k}", String::from_utf8_lossy(p)))
            .collect::<Vec<_>>()
            .join(","),
    }
}

/// Compare a real outcome with the model's, returning a divergence description.
fn compare<T: std::fmt::Debug, M: std::fmt::Debug>(
    what: &str,
    real: &Result<T, tonic::Status>,
    model: &Result<M, MErr>,
    same_ok: impl Fn(&T, &M) -> Result<(), String>,
) -> Result<(), String> {
    match (real, model) {
        (Ok(r), Ok(m)) => same_ok(r, m).map_err(|d| format!("{what}: {d}")),
        (Err(s), Err(m)) => {
            let (code, sub, conflicts) = real_err(s);
            if code != m.code {
                return Err(format!(
                    "{what}: error code {code} (real) vs {} (model); {s:?}",
                    m.code
                ));
            }
            if let Some(ms) = m.subcode
                && sub.as_deref() != Some(ms)
            {
                return Err(format!("{what}: subcode {sub:?} (real) vs {ms} (model)"));
            }
            if m.conflicts.is_some() && conflicts != m.conflicts {
                return Err(format!(
                    "{what}: conflicts [{}] (real) vs [{}] (model)",
                    show_conflicts(&conflicts),
                    show_conflicts(&m.conflicts)
                ));
            }
            Ok(())
        }
        (Ok(r), Err(m)) => Err(format!(
            "{what}: real succeeded ({r:?}), model failed {m:?}"
        )),
        (Err(s), Ok(m)) => Err(format!(
            "{what}: real failed {:?} ({s:?}), model succeeded ({m:?})",
            real_err(s)
        )),
    }
}

fn same_id(real: &[u8], model: &Id) -> Result<(), String> {
    if real == model.as_slice() {
        Ok(())
    } else {
        Err(format!(
            "id {} (real) vs {} (model)",
            text(&raw(real)),
            text(model)
        ))
    }
}

/// The flattened tree of changeset `cs` through `ListTree`: path -> (mode, blob id, size).
async fn flatten(client: &ScionClient, cs: &Id) -> BTreeMap<Vec<u8>, (u32, Id, u64)> {
    let mut out = BTreeMap::new();
    let mut dirs: Vec<Vec<u8>> = vec![Vec::new()];
    while let Some(dir) = dirs.pop() {
        let r = client
            .query()
            .list_tree(v1::ListTreeRequest {
                changeset: idb(cs),
                path: dir.clone(),
            })
            .await
            .unwrap_or_else(|s| panic!("ListTree {}: {s:?}", String::from_utf8_lossy(&dir)))
            .into_inner();
        for e in r.entries {
            let mut p = dir.clone();
            if !p.is_empty() {
                p.push(b'/');
            }
            p.extend_from_slice(&e.name);
            let info = e.entry.expect("entry info");
            if info.mode == 4 {
                dirs.push(p);
            } else {
                out.insert(p, (info.mode, raw(&info.id), info.size));
            }
        }
    }
    out
}

fn flatten_model(t: &MTree) -> BTreeMap<Vec<u8>, (u32, Id, u64)> {
    t.iter()
        .map(|(p, f)| {
            (
                p.clone(),
                (
                    u32::from(f.mode),
                    blob_id(&f.content),
                    f.content.len() as u64,
                ),
            )
        })
        .collect()
}

// ---------------------------------------------------------------------------------------
// The driver.
// ---------------------------------------------------------------------------------------

struct Seed<'a> {
    env: &'a Env,
    seed: u64,
    rng: ChaCha8Rng,
    model: Model,
    uploaded: BTreeSet<Vec<u8>>,
    counter: u64,
    /// Change ids of created changesets (for amended revisions).
    changes: Vec<[u8; 16]>,
    /// Negative control: the model forgets redactions (the differential must notice).
    ignore_redactions: bool,
}

impl<'a> Seed<'a> {
    fn ref_name(&self, r: &str) -> String {
        format!("d/{}/{r}", self.seed)
    }

    fn server(&mut self) -> usize {
        self.rng.random_range(0..SERVERS)
    }

    fn tag(&mut self) -> String {
        self.counter += 1;
        format!("s{}k{}", self.seed, self.counter)
    }

    fn fresh_change(&mut self) -> [u8; 16] {
        self.counter += 1;
        let mut c = [0u8; 16];
        c[..8].copy_from_slice(&self.seed.to_be_bytes());
        c[8..].copy_from_slice(&self.counter.to_be_bytes());
        c[0] |= 0x40; // never the TestCluster or root change ids
        c
    }

    /// A random known changeset: recent ones and ref targets preferred.
    fn pick(&mut self) -> usize {
        let n = self.model.commits.len();
        match self.rng.random_range(0..10) {
            0..=4 => n - 1 - self.rng.random_range(0..n.min(4)),
            5..=6 => {
                let r = REFS[self.rng.random_range(0..REFS.len())];
                self.model.refs[&self.ref_name(r)].target
            }
            _ => self.rng.random_range(0..n),
        }
    }

    async fn upload(&mut self, delta: &[MDelta]) {
        let mut fresh: Vec<Vec<u8>> = Vec::new();
        for d in delta {
            if let MDelta::Put(_, f) = d
                && !self.model.redacted.contains(&f.content)
                && self.uploaded.insert(f.content.clone())
            {
                fresh.push(f.content.clone());
            }
        }
        if fresh.is_empty() {
            return;
        }
        let s = self.server();
        let refs: Vec<&[u8]> = fresh.iter().map(Vec::as_slice).collect();
        TestCluster::upload(&self.env.alice[s], &refs).await;
    }

    async fn start(&mut self) -> Result<(), String> {
        for r in REFS {
            let name = self.ref_name(r);
            let resp = self.env.admin[0]
                .refs()
                .create_ref(v1::CreateRefRequest {
                    name: name.clone(),
                    target: idb(&self.env.root),
                    reason: "differential".into(),
                })
                .await
                .map_err(|s| format!("CreateRef {name}: {s:?}"))?
                .into_inner();
            if resp.version != 1 {
                return Err(format!("CreateRef {name}: version {}", resp.version));
            }
            self.model.refs.insert(
                name,
                MRef {
                    version: 1,
                    target: 0,
                    log: vec![MLogEntry {
                        version: 1,
                        old: None,
                        new: self.env.root,
                        landed: Vec::new(),
                    }],
                },
            );
        }
        Ok(())
    }

    async fn op_create(&mut self, k: usize) -> Result<(), String> {
        let parent = self.pick();
        let ptree = self.model.commits[parent].tree.clone();
        let tag = self.tag();
        let (delta, kind) = match self.rng.random_range(0..100) {
            0..=3 => {
                // Delete a path that is not a file.
                let p = PATHS
                    .iter()
                    .map(|p| p.as_bytes().to_vec())
                    .find(|p| !ptree.contains_key(p))
                    .unwrap_or_else(|| b"nowhere.txt".to_vec());
                (vec![MDelta::Delete(p)], "create_bad_delete")
            }
            4..=6 if !ptree.is_empty() => {
                let (p, f) = ptree.iter().next().expect("a file");
                (vec![MDelta::Put(p.clone(), f.clone())], "create_noop_put")
            }
            7..=9 if !ptree.is_empty() => {
                let p = ptree.keys().last().expect("a file").clone();
                let mut under = p.clone();
                under.extend_from_slice(b"/under.txt");
                let f = MFile {
                    mode: 1,
                    content: format!("{tag} under\n").into_bytes(),
                };
                (vec![MDelta::Put(under, f)], "create_file_directory")
            }
            10..=12 if !self.model.redacted.is_empty() => {
                let all: Vec<&Vec<u8>> = self.model.redacted.iter().collect();
                let content = all[self.rng.random_range(0..all.len())].clone();
                let p = format!("r/{tag}.txt").into_bytes();
                let mut t = ptree.clone();
                t.retain(|q, _| q.as_slice() != b"r");
                t.insert(p, MFile { mode: 1, content });
                (file_diff(&ptree, &t), "create_redacted_put")
            }
            _ => {
                let t = mutate(&ptree, &mut self.rng, &tag);
                (file_diff(&ptree, &t), "create")
            }
        };
        let change = if !self.changes.is_empty() && self.rng.random_range(0..100) < 15 {
            self.changes[self.rng.random_range(0..self.changes.len())]
        } else {
            self.fresh_change()
        };
        let msg = format!("seed {} op {k}", self.seed).into_bytes();
        let time = 1_700_000_000 + self.counter as i64;
        let meta = self.model.meta(&msg, time, &alice());
        let model = self.model.create(parent, &delta, meta, change);
        self.upload(&delta).await;
        let s = self.server();
        let real = self.env.alice[s]
            .write()
            .create_changeset(v1::CreateChangesetRequest {
                parents: vec![idb(&self.model.commits[parent].id)],
                delta: to_proto(&delta),
                meta_fields: Some(meta_fields(&msg, time)),
                change_id: change.to_vec(),
                expected_root_tree: None,
                import_mode: false,
            })
            .await
            .map(|r| r.into_inner());
        compare(&format!("op {k} {kind}"), &real, &model, |r, m| {
            same_id(&r.changeset_id, &m.id)
        })?;
        self.env.count(&format!(
            "{kind}:{}",
            if model.is_ok() { "ok" } else { "err" }
        ));
        if let Ok(c) = model {
            self.changes.push(c.change);
            self.model.add(c);
        }
        Ok(())
    }

    async fn op_rebase(&mut self, k: usize) -> Result<(), String> {
        if self.model.commits.len() < 2 {
            return Ok(());
        }
        let c = 1 + self.rng.random_range(0..self.model.commits.len() - 1);
        let c = if self.rng.random_bool(0.5) {
            self.model.commits.len() - 1
        } else {
            c
        };
        let onto = self.pick();
        let model = self.model.rebase(c, onto);
        let s = self.server();
        let real = self.env.alice[s]
            .write()
            .rebase(v1::RebaseRequest {
                changeset: idb(&self.model.commits[c].id),
                onto: idb(&self.model.commits[onto].id),
            })
            .await
            .map(|r| r.into_inner());
        let expected = model
            .as_ref()
            .map(|r| match r {
                Rebased::Same(i) => self.model.commits[*i].id,
                Rebased::New(m) => m.id,
            })
            .map_err(|e| e.clone());
        compare(&format!("op {k} rebase"), &real, &expected, |r, m| {
            same_id(&r.changeset_id, m)
        })?;
        self.env.count(&match &model {
            Ok(_) => "rebase:ok".to_owned(),
            Err(e) => format!("rebase:{}", e.code),
        });
        if let Ok(r) = model {
            self.model.register(r);
        }
        Ok(())
    }

    async fn op_merge(&mut self, k: usize) -> Result<(), String> {
        let a = self.pick();
        let b = self.pick();
        if a == b {
            return Ok(());
        }
        let change = self.fresh_change();
        let msg = format!("seed {} merge {k}", self.seed).into_bytes();
        let time = 1_700_000_000 + self.counter as i64;
        let meta = self.model.meta(&msg, time, &alice());
        let model = self.model.merge(a, b, meta, change);
        let s = self.server();
        let real = self.env.alice[s]
            .write()
            .merge(v1::MergeRequest {
                a: idb(&self.model.commits[a].id),
                b: idb(&self.model.commits[b].id),
                meta_fields: Some(meta_fields(&msg, time)),
                change_id: change.to_vec(),
            })
            .await
            .map(|r| r.into_inner());
        compare(&format!("op {k} merge"), &real, &model, |r, m| {
            same_id(&r.changeset_id, &m.id)
        })?;
        self.env.count(&match &model {
            Ok(_) => "merge:ok".to_owned(),
            Err(e) => format!("merge:{}", e.code),
        });
        if let Ok(c) = model {
            self.changes.push(c.change);
            self.model.add(c);
        }
        Ok(())
    }

    async fn op_land(&mut self, k: usize) -> Result<(), String> {
        let which = REFS[self.rng.random_range(0..REFS.len())];
        let name = self.ref_name(which);
        let head = if self.rng.random_bool(0.6) {
            self.model.commits.len() - 1 - self.rng.random_range(0..self.model.commits.len().min(3))
        } else {
            self.pick()
        };
        let allow_empty = self.rng.random_range(0..5) == 0;
        let model = self.model.land(&name, head, allow_empty);
        let s = self.server();
        let real = self.env.alice[s]
            .write()
            .land(v1::LandRequest {
                r#ref: name.clone(),
                head: idb(&self.model.commits[head].id),
                allow_empty,
            })
            .await
            .map(|r| r.into_inner());
        compare(&format!("op {k} land {name}"), &real, &model, |r, m| {
            same_id(&r.target, &m.0)?;
            if r.version != m.1 {
                return Err(format!("version {} (real) vs {} (model)", r.version, m.1));
            }
            let got: Vec<MLanded> = r
                .landed
                .iter()
                .map(|l| MLanded {
                    change: l.change_id.clone().try_into().unwrap_or([0; 16]),
                    source: raw(&l.source),
                    result: raw(&l.result),
                })
                .collect();
            if got != m.2 {
                return Err(format!("landed {got:?} (real) vs {:?} (model)", m.2));
            }
            Ok(())
        })?;
        self.env.count(&match &model {
            Ok((_, _, l)) if l.is_empty() => "land:noop".to_owned(),
            Ok(_) => "land:ok".to_owned(),
            Err(e) => format!("land:{}", e.code),
        });
        Ok(())
    }

    async fn op_update_ref(&mut self, k: usize) -> Result<(), String> {
        let which = REFS[self.rng.random_range(0..REFS.len())];
        let name = self.ref_name(which);
        let (v, cur) = {
            let r = &self.model.refs[&name];
            (r.version, r.target)
        };
        let expected = match self.rng.random_range(0..10) {
            0 => v - 1,
            1 => v + 1,
            _ => v,
        };
        let target = self.pick();
        let ff = self.model.repo.is_ancestor(cur, target);
        // Fast-forward updates by alice; otherwise alice without or with the flag (no force
        // permission) or an admin with the flag.
        let (as_admin, flag) = if ff {
            (false, false)
        } else {
            match self.rng.random_range(0..3) {
                0 => (false, false),
                1 => (false, true),
                _ => (true, true),
            }
        };
        let model = self
            .model
            .update_ref(&name, expected, target, as_admin && flag);
        let s = self.server();
        let client = if as_admin {
            &self.env.admin[s]
        } else {
            &self.env.alice[s]
        };
        let real = client
            .refs()
            .update_ref(v1::UpdateRefRequest {
                name: name.clone(),
                expected_version: expected,
                new_target: idb(&self.model.commits[target].id),
                allow_non_fast_forward: flag,
            })
            .await
            .map(|r| r.into_inner());
        compare(
            &format!("op {k} update_ref {name}"),
            &real,
            &model,
            |r, m| {
                if r.version == *m {
                    Ok(())
                } else {
                    Err(format!("version {} (real) vs {m} (model)", r.version))
                }
            },
        )?;
        self.env.count(&match &model {
            Ok(_) => "update_ref:ok".to_owned(),
            Err(e) => format!("update_ref:{}", e.code),
        });
        Ok(())
    }

    async fn op_redact(&mut self, k: usize) -> Result<(), String> {
        let candidates: Vec<Vec<u8>> = self
            .model
            .contents()
            .into_iter()
            .filter(|c| !self.model.redacted.contains(c))
            .collect();
        if candidates.is_empty() {
            return Ok(());
        }
        let content = candidates[self.rng.random_range(0..candidates.len())].clone();
        let s = self.server();
        let r = self.env.admin[s]
            .admin()
            .redact(v1::RedactRequest {
                object_ids: vec![blob_id(&content).to_vec()],
                reason: format!("differential seed {}", self.seed),
                purge_after_seconds: None,
            })
            .await
            .map_err(|s| format!("op {k} redact: {s:?}"))?
            .into_inner();
        if !self.ignore_redactions {
            self.model.redacted.insert(content);
        }
        self.env.await_redaction_epoch(r.epoch).await;
        self.env.count("redact:ok");
        Ok(())
    }

    /// Refs, ref logs and flattened trees of the ref targets.
    async fn final_check(&mut self) -> Result<(), String> {
        for r in REFS {
            let name = self.ref_name(r);
            let m = self.model.refs[&name].clone();
            let s = self.server();
            let got = self.env.alice[s]
                .objects()
                .resolve_ref(v1::ResolveRefRequest {
                    name: name.clone(),
                    at_version: None,
                })
                .await
                .map_err(|e| format!("ResolveRef {name}: {e:?}"))?
                .into_inner();
            let target = self.model.commits[m.target].id;
            if got.version != m.version || got.target != target.to_vec() {
                return Err(format!(
                    "ref {name}: ({}, {}) (real) vs ({}, {}) (model)",
                    got.version,
                    text(&raw(&got.target)),
                    m.version,
                    text(&target)
                ));
            }
            let log = self.env.alice[s]
                .refs()
                .get_ref_log(v1::GetRefLogRequest {
                    name: name.clone(),
                    from_version: 0,
                    limit: 0,
                })
                .await
                .map_err(|e| format!("GetRefLog {name}: {e:?}"))?
                .into_inner()
                .entries;
            let real_log: Vec<MLogEntry> = log
                .iter()
                .map(|e| MLogEntry {
                    version: e.version,
                    old: (!e.old_target.is_empty()).then(|| raw(&e.old_target)),
                    new: raw(&e.new_target),
                    landed: e
                        .landed
                        .iter()
                        .map(|l| MLanded {
                            change: l.change_id.clone().try_into().unwrap_or([0; 16]),
                            source: raw(&l.source),
                            result: raw(&l.result),
                        })
                        .collect(),
                })
                .collect();
            if real_log != m.log {
                return Err(format!(
                    "ref log of {name}: {real_log:?} (real) vs {:?} (model)",
                    m.log
                ));
            }
            let flat = flatten(&self.env.alice[s], &target).await;
            let want = flatten_model(&self.model.commits[m.target].tree);
            if flat != want {
                return Err(format!(
                    "flattened tree of {name} ({}): {flat:?} (real) vs {want:?} (model)",
                    text(&target)
                ));
            }
        }
        // One more random changeset, flattened.
        let i = self.rng.random_range(0..self.model.commits.len());
        let s = self.server();
        let flat = flatten(&self.env.alice[s], &self.model.commits[i].id).await;
        if flat != flatten_model(&self.model.commits[i].tree) {
            return Err(format!(
                "flattened tree of {}",
                text(&self.model.commits[i].id)
            ));
        }
        Ok(())
    }
}

async fn run_seed(env: &Env, seed: u64, ops: usize, ignore_redactions: bool) -> Result<(), String> {
    let mut sd = Seed {
        env,
        seed,
        rng: ChaCha8Rng::seed_from_u64(seed),
        model: Model::new(env.root),
        uploaded: BTreeSet::new(),
        counter: 0,
        changes: Vec::new(),
        ignore_redactions,
    };
    sd.start().await?;
    for k in 0..ops {
        let roll = sd.rng.random_range(0..100);
        let r = match roll {
            0..=34 => sd.op_create(k).await,
            35..=49 => sd.op_rebase(k).await,
            50..=59 => sd.op_merge(k).await,
            60..=81 => sd.op_land(k).await,
            82..=95 => sd.op_update_ref(k).await,
            _ => sd.op_redact(k).await,
        };
        r.map_err(|d| format!("seed {seed}: {d}"))?;
    }
    sd.final_check()
        .await
        .map_err(|d| format!("seed {seed}: final state: {d}"))
}

/// Run `seeds` concurrently on `env`, returning every seed's divergence.
async fn divergences(
    env: &Env,
    seeds: std::ops::Range<u64>,
    ops: usize,
    ignore_redactions: bool,
) -> Vec<String> {
    futures::stream::iter(seeds)
        .map(|seed| run_seed(env, seed, ops, ignore_redactions))
        .buffer_unordered(PARALLEL)
        .filter_map(|r| async move { r.err() })
        .collect()
        .await
}

async fn run_differential(backend: Backend, seeds: std::ops::Range<u64>, ops: usize) {
    let env = Env::start(backend).await;
    let n = seeds.end - seeds.start;
    let divergences = divergences(&env, seeds, ops, false).await;
    let stats = env.stats.lock().unwrap().clone();
    println!("differential over {n} seeds: {stats:?}");
    assert!(
        divergences.is_empty(),
        "{} of {n} seeds diverged from the reference model; first: {:#?}",
        divergences.len(),
        divergences.iter().take(3).collect::<Vec<_>>()
    );
    // The driver exercises every operation and outcome class.
    for key in [
        "create:ok",
        "create_bad_delete:err",
        "create_noop_put:err",
        "create_file_directory:err",
        "create_redacted_put:err",
        "rebase:ok",
        "rebase:MERGE_CONFLICT",
        "rebase:EMPTY_CHANGE",
        "merge:ok",
        "merge:MERGE_CONFLICT",
        "land:ok",
        "land:MERGE_CONFLICT",
        "land:ALREADY_LANDED",
        "update_ref:ok",
        "update_ref:REF_VERSION_MISMATCH",
        "update_ref:NOT_FAST_FORWARD",
        "redact:ok",
    ] {
        if n >= 100 {
            assert!(
                stats.get(key).copied().unwrap_or(0) > 0,
                "the driver never produced {key}"
            );
        }
    }
    invariants::check_16_2(&env.c).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn whole_system_matches_the_reference_model() {
    run_differential(Backend::Memory, 0..200, 30).await;
}

/// Negative control: a model that ignores redactions must diverge, so the comparisons are
/// sensitive (the same seeds agree with the faithful model above).
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn differential_detects_a_model_that_ignores_redactions() {
    let env = Env::start(Backend::Memory).await;
    let found = divergences(&env, 0..40, 30, true).await;
    assert!(
        !found.is_empty(),
        "a model that forgets redactions went unnoticed over 40 seeds"
    );
    println!(
        "negative control: {} divergences, e.g. {}",
        found.len(),
        found[0]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
#[ignore = "spec-sized seed count; run with SCION_FULL=1 --include-ignored"]
async fn full_whole_system_matches_the_reference_model() {
    if skip_full() {
        return;
    }
    run_differential(Backend::Memory, 10_000..12_000, 50).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn external_whole_system_matches_the_reference_model() {
    if skip_external() {
        return;
    }
    run_differential(Backend::External, 0..40, 30).await;
}

// ---------------------------------------------------------------------------------------
// SPEC 19.4: rebase determinism across server processes and after restart.
// ---------------------------------------------------------------------------------------

/// `(C, X)` on a shared base: C and X edit different lines of one file and add files, so
/// the rebase needs a content merge. Returns `(c, x, model id of Rebase(C, X))`.
async fn rebase_inputs(c: &TestCluster) -> (ObjectId, ObjectId, Id) {
    let client = c.client(scion_conformance::Via::Server, &alice()).await;
    let (root, _) = c.ref_head("main").await;
    let base_content = b"one\ntwo\nthree\nfour\nfive\nsix\n";
    let base = TestCluster::commit(&client, &root, &[("f.txt", base_content)], &[], 1).await;
    let cc = b"ONE\ntwo\nthree\nfour\nfive\nsix\n";
    let xc = b"one\ntwo\nthree\nfour\nfive\nSIX\n";
    let c_id =
        TestCluster::commit(&client, &base, &[("f.txt", cc), ("c.txt", b"c\n")], &[], 2).await;
    let x_id =
        TestCluster::commit(&client, &base, &[("f.txt", xc), ("x.txt", b"x\n")], &[], 3).await;

    // The model's id of the rebase result.
    let mut model = Model::new(*root.as_bytes());
    let file = |c: &[u8]| MFile {
        mode: 1,
        content: c.to_vec(),
    };
    let meta = |n: u32| model_meta(&format!("change {n}"));
    let b_tree: MTree = [(b"f.txt".to_vec(), file(base_content))].into();
    let mk = |model: &mut Model, parent: usize, tree: MTree, n: u32| {
        let change: [u8; 16] = TestCluster::change_id(n).try_into().unwrap();
        let cs = changeset_for(
            vec![model.commits[parent].id],
            &model.commits[parent].tree.clone(),
            &tree,
            meta(n),
            change,
        );
        model.add(MCommit {
            id: changeset_id(&cs),
            parents: vec![parent],
            tree,
            meta: meta(n),
            change,
        })
    };
    let b = mk(&mut model, 0, b_tree.clone(), 1);
    assert_eq!(model.commits[b].id, *base.as_bytes(), "model base id");
    let mut ct = b_tree.clone();
    ct.insert(b"f.txt".to_vec(), file(cc));
    ct.insert(b"c.txt".to_vec(), file(b"c\n"));
    let ci = mk(&mut model, b, ct, 2);
    assert_eq!(model.commits[ci].id, *c_id.as_bytes(), "model C id");
    let mut xt = b_tree;
    xt.insert(b"f.txt".to_vec(), file(xc));
    xt.insert(b"x.txt".to_vec(), file(b"x\n"));
    let xi = mk(&mut model, b, xt, 3);
    assert_eq!(model.commits[xi].id, *x_id.as_bytes(), "model X id");
    let Ok(Rebased::New(r)) = model.rebase(ci, xi) else {
        panic!("the model rebase must be clean");
    };
    assert_eq!(
        r.tree[b"f.txt".as_slice()].content,
        b"ONE\ntwo\nthree\nfour\nfive\nSIX\n".to_vec()
    );
    (c_id, x_id, r.id)
}

/// Meta id of [`TestCluster::meta`] created by alice.
fn model_meta(msg: &str) -> Id {
    meta_id(&MMeta {
        author: b"Conformance <conformance@example.com>".to_vec(),
        author_time: (1_700_000_000, 0),
        committer: None,
        committer_time: None,
        creator: alice().id_string().into_bytes(),
        message: msg.as_bytes().to_vec(),
        ext: Vec::new(),
    })
}

async fn rebase_on(c: &TestCluster, i: usize, cs: &ObjectId, onto: &ObjectId) -> Id {
    let s = &c.servers[i];
    let client = ScionClient::connect(&s.endpoint, s.tls.clone(), Some(c.token_for(&alice())))
        .await
        .expect("connect");
    let r = client
        .write()
        .rebase(v1::RebaseRequest {
            changeset: cs.as_bytes().to_vec(),
            onto: onto.as_bytes().to_vec(),
        })
        .await
        .unwrap_or_else(|s| panic!("Rebase on server {i}: {s:?}"))
        .into_inner();
    raw(&r.changeset_id)
}

async fn run_rebase_determinism(backend: Backend) {
    let mut c = TestCluster::start(
        ClusterOpts::new(backend.clone())
            .with_servers(SERVERS)
            .with_daemon(false),
    )
    .await;
    let (cs, onto, want) = rebase_inputs(&c).await;
    // Concurrently on every server process.
    let ids = futures::future::join_all((0..SERVERS).map(|i| rebase_on(&c, i, &cs, &onto))).await;
    for (i, id) in ids.iter().enumerate() {
        assert_eq!(text(id), text(&want), "server {i}");
    }
    // After every server restarted.
    for i in 0..SERVERS {
        c.restart_server(i).await;
    }
    for i in 0..SERVERS {
        assert_eq!(
            text(&rebase_on(&c, i, &cs, &onto).await),
            text(&want),
            "server {i} after restart"
        );
    }
    // After a kill and a later restart (a fresh process over the same storage).
    c.kill_server(1).await;
    assert_eq!(text(&rebase_on(&c, 2, &cs, &onto).await), text(&want));
    c.restart_server(1).await;
    assert_eq!(text(&rebase_on(&c, 1, &cs, &onto).await), text(&want));
    invariants::check_16_2(&c).await.unwrap();

    // An independent cluster (separate storage, separate processes): the same inputs give
    // the same ids.
    let other = TestCluster::start(ClusterOpts::new(backend).with_daemon(false)).await;
    let (cs2, onto2, want2) = rebase_inputs(&other).await;
    assert_eq!(
        (cs2, onto2),
        (cs, onto),
        "identical creates give identical ids"
    );
    assert_eq!(want2, want);
    assert_eq!(text(&rebase_on(&other, 0, &cs2, &onto2).await), text(&want));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn rebase_is_deterministic_across_server_processes_and_restarts() {
    run_rebase_determinism(Backend::Memory).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn external_rebase_is_deterministic_across_server_processes_and_restarts() {
    if skip_external() {
        return;
    }
    run_rebase_determinism(Backend::External).await;
}
