//! The CockroachDB metadata store (SPEC 5.2-5.4; `docs/contracts/storage-semantics.md`).
//!
//! Every [`MetaStore`] method is one `SERIALIZABLE` transaction on a pooled connection
//! (deadpool-postgres over tokio-postgres). SQLSTATE `40001` (and `40003` on read-only methods)
//! is retried client-side with full-jitter backoff, at most [`txn::MAX_ATTEMPTS`] attempts;
//! exhaustion is UNAVAILABLE/txn_retry. Connection and pool failures are
//! UNAVAILABLE/store_unavailable. [`Consistency::Stale`] reads run as a single statement with
//! `AS OF SYSTEM TIME follower_read_timestamp()` outside an explicit transaction.

mod codec;
mod txn;

use crate::config::CrdbConfig;
use crate::migrations::MIGRATIONS;
use async_trait::async_trait;
use codec::*;
use deadpool_postgres::{Manager, ManagerConfig, Object, Pool, RecyclingMethod, Runtime};
use scion_core::{ChangeId, ErrorCode, ObjectId, Result, ScionError, subcode};
use scion_object::RefName;
use scion_store::*;
use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime};
use tokio_postgres::{NoTls, Row};
use txn::{
    MAX_ATTEMPTS, Params, TxErr, backoff, begin, execute, is_schema_too_new, is_unique_violation,
    query, query_implicit,
};

/// Rows per multi-row `INSERT ... SELECT FROM UNNEST(...)` statement.
const BATCH_ROWS: usize = 1000;
/// Pool size used by [`CrdbMetaStore::connect_fresh_for_tests`].
const TEST_POOL_SIZE: usize = 32;
/// Repository name used by [`CrdbMetaStore::connect_fresh_for_tests`] (as `MemOptions`).
const TEST_REPO_NAME: &str = "test";

/// The follower-read clause used for [`Consistency::Stale`] (storage-semantics 1.7, 3).
macro_rules! stale_variant {
    ($head:literal, $tail:literal) => {
        (
            concat!($head, " ", $tail),
            concat!(
                $head,
                " AS OF SYSTEM TIME follower_read_timestamp() ",
                $tail
            ),
        )
    };
}

/// `(strong, stale)` SQL for `get_objects`.
const GET_OBJECTS: (&str, &str) = stale_variant!(
    "SELECT object_id, kind, size, stored_size, inline_data, state, created_at FROM objects",
    "WHERE repo_id = $1 AND object_id = ANY($2)"
);
/// `(strong, stale)` SQL for `get_changesets`.
const GET_CHANGESETS: (&str, &str) = stale_variant!(
    "SELECT changeset_id, encoded, generation, skip, change_id, created_by, created_at FROM changesets",
    "WHERE repo_id = $1 AND changeset_id = ANY($2)"
);
/// `(strong, stale)` SQL for `get_derived`.
const GET_DERIVED: (&str, &str) = stale_variant!(
    "SELECT value, storage_key, source_object, at FROM derived",
    "WHERE repo_id = $1 AND type = $2 AND type_version = $3 AND key = $4"
);

const SELECT_REF: &str =
    "SELECT name, target, version, updated_at FROM refs WHERE repo_id = $1 AND name = $2";
const SELECT_REF_FOR_UPDATE: &str = "SELECT name, target, version, updated_at FROM refs \
     WHERE repo_id = $1 AND name = $2 FOR UPDATE";
const SELECT_LANDED: &str = "SELECT ref, change_id, version, source_changeset, result_changeset \
     FROM landed WHERE repo_id = $1 AND ref = $2 AND change_id = $3";
const SELECT_EPOCH_FOR_UPDATE: &str =
    "SELECT value FROM counters WHERE repo_id = $1 AND name = 'redaction_epoch' FOR UPDATE";
const UPSERT_EPOCH: &str =
    "UPSERT INTO counters (repo_id, name, value) VALUES ($1, 'redaction_epoch', $2)";
const INSERT_AUDIT: &str = "INSERT INTO audit_log (repo_id, at, actor, action, target, reason, \
     detail) VALUES ($1, $2, $3, $4, $5, $6, $7)";

/// Shared state behind the store handle.
struct Inner {
    pool: Pool,
    repo_id: RepoId,
    repo_name: String,
    /// The database name from the connection configuration, when given.
    database: Option<String>,
    /// Retried attempts (40001/40003/conflicts) since construction.
    retries: AtomicU64,
}

/// SERIALIZABLE transactions, bounded 40001 retry, `AS OF SYSTEM TIME
/// follower_read_timestamp()` for [`Consistency::Stale`] (SPEC 5.2-5.4; PLAN 2.4).
#[derive(Clone)]
pub struct CrdbMetaStore {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for CrdbMetaStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CrdbMetaStore")
            .field("repo_id", &self.inner.repo_id)
            .field("repo_name", &self.inner.repo_name)
            .finish_non_exhaustive()
    }
}

/// `repo_id = i64::from_be_bytes(BLAKE3(repo_name)[0..8]) & i64::MAX`
/// (storage-semantics 1.5; identical to the in-memory store).
fn compute_repo_id(name: &str) -> RepoId {
    let h = ObjectId::hash_blob(name.as_bytes());
    let mut b = [0u8; 8];
    b.copy_from_slice(&h.digest()[..8]);
    RepoId(i64::from_be_bytes(b) & i64::MAX)
}

fn unavailable(what: impl std::fmt::Display) -> ScionError {
    ScionError::unavailable(subcode::STORE_UNAVAILABLE, what.to_string())
}

fn ids_bytes(ids: &[ObjectId]) -> Vec<&[u8]> {
    ids.iter().map(|i| i.as_bytes().as_slice()).collect()
}

fn dedup_ids(ids: &[ObjectId]) -> Vec<ObjectId> {
    ids.iter()
        .copied()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

/// Column vectors of a `derived` batch insert.
struct DerivedCols {
    tys: Vec<String>,
    versions: Vec<i64>,
    keys: Vec<Vec<u8>>,
    values: Vec<Option<Vec<u8>>>,
    storage_keys: Vec<Option<String>>,
    sources: Vec<Option<Vec<u8>>>,
    ats: Vec<SystemTime>,
}

impl DerivedCols {
    fn new(rows: &[DerivedRow]) -> Self {
        DerivedCols {
            tys: rows.iter().map(|r| r.key.ty.clone()).collect(),
            versions: rows.iter().map(|r| i64::from(r.key.version)).collect(),
            keys: rows.iter().map(|r| r.key.key.to_vec()).collect(),
            values: rows
                .iter()
                .map(|r| r.value.as_ref().map(|v| v.to_vec()))
                .collect(),
            storage_keys: rows.iter().map(|r| r.storage_key.clone()).collect(),
            sources: rows
                .iter()
                .map(|r| r.source_object.map(|s| s.as_bytes().to_vec()))
                .collect(),
            ats: rows.iter().map(|r| r.at).collect(),
        }
    }

    fn len(&self) -> usize {
        self.tys.len()
    }
}

/// `INSERT INTO derived ... ON CONFLICT DO NOTHING` for every row, in batches.
async fn insert_derived(
    tx: &deadpool_postgres::Transaction<'_>,
    rid: i64,
    cols: &DerivedCols,
) -> std::result::Result<(), TxErr> {
    const SQL: &str = "INSERT INTO derived (repo_id, type, type_version, key, value, storage_key, \
         source_object, at) SELECT $1, u.t, u.v, u.k, u.val, u.sk, u.src, u.a FROM \
         UNNEST($2::STRING[], $3::INT8[], $4::BYTES[], $5::BYTES[], $6::STRING[], $7::BYTES[], \
         $8::TIMESTAMPTZ[]) AS u(t, v, k, val, sk, src, a) ON CONFLICT DO NOTHING";
    let mut start = 0;
    while start < cols.len() {
        let end = (start + BATCH_ROWS).min(cols.len());
        let (t, v, k) = (
            &cols.tys[start..end],
            &cols.versions[start..end],
            &cols.keys[start..end],
        );
        let (val, sk, src, a) = (
            &cols.values[start..end],
            &cols.storage_keys[start..end],
            &cols.sources[start..end],
            &cols.ats[start..end],
        );
        execute(tx, SQL, &[&rid, &t, &v, &k, &val, &sk, &src, &a]).await?;
        start = end;
    }
    Ok(())
}

async fn insert_audit(
    tx: &deadpool_postgres::Transaction<'_>,
    rid: i64,
    a: &AuditRow,
) -> std::result::Result<(), TxErr> {
    execute(
        tx,
        INSERT_AUDIT,
        &[
            &rid, &a.at, &a.actor, &a.action, &a.target, &a.reason, &a.detail,
        ],
    )
    .await?;
    Ok(())
}

/// The current redaction epoch, locked for update (0 when the counter row is absent).
async fn locked_epoch(
    tx: &deadpool_postgres::Transaction<'_>,
    rid: i64,
) -> std::result::Result<u64, TxErr> {
    let rows = query(tx, SELECT_EPOCH_FOR_UPDATE, &[&rid]).await?;
    match rows.first() {
        Some(r) => Ok(to_u64(col(r, 0)?, "redaction epoch")?),
        None => Ok(0),
    }
}

async fn write_epoch(
    tx: &deadpool_postgres::Transaction<'_>,
    rid: i64,
    epoch: u64,
) -> std::result::Result<(), TxErr> {
    let e = to_i64(epoch, "redaction epoch")?;
    execute(tx, UPSERT_EPOCH, &[&rid, &e]).await?;
    Ok(())
}

impl CrdbMetaStore {
    /// Connect, apply [`crate::MIGRATIONS`] (idempotent) and ensure the `repos` row
    /// (storage-semantics 2: `INSERT ... ON CONFLICT DO NOTHING`). An unparsable URL is
    /// INVALID_ARGUMENT; an unreachable database is UNAVAILABLE/store_unavailable (SPEC 16.1
    /// class 1).
    pub async fn connect(cfg: CrdbConfig) -> Result<Self> {
        let pg: tokio_postgres::Config = cfg.url.parse().map_err(|e| {
            ScionError::new(
                ErrorCode::InvalidArgument,
                format!("invalid metadata url: {e}"),
            )
        })?;
        let store = Self::from_pg_config(pg, cfg.tls.clone(), cfg.max_connections, &cfg.repo_name)?;
        store.migrate().await?;
        store.ensure_repo().await?;
        Ok(store)
    }

    /// Apply [`crate::MIGRATIONS`] (idempotent: `CREATE ... IF NOT EXISTS`).
    pub async fn migrate(&self) -> Result<()> {
        for &(name, sql) in MIGRATIONS {
            self.txn("migrate", false, |c| async move {
                c.batch_execute(sql).await?;
                Ok(())
            })
            .await?;
            tracing::debug!(target: "scion::store::crdb", migration = name, "migration applied");
        }
        Ok(())
    }

    /// `CREATE DATABASE scion_test_<rand>`, connect and migrate (tests; PLAN 3.3). The base URL
    /// names any existing database (e.g. `defaultdb`) and is used without TLS.
    pub async fn connect_fresh_for_tests(base_url: &str) -> Result<Self> {
        let base: tokio_postgres::Config = base_url.parse().map_err(|e| {
            ScionError::new(
                ErrorCode::InvalidArgument,
                format!("invalid test database url: {e}"),
            )
        })?;
        let db = format!("scion_test_{:016x}", rand::random::<u64>());
        let (client, conn) = base
            .connect(NoTls)
            .await
            .map_err(|e| unavailable(format!("connect to test database: {e}")))?;
        let conn_task = tokio::spawn(conn);
        let created = client
            .batch_execute(&format!("CREATE DATABASE {db}"))
            .await
            .map_err(|e| unavailable(format!("create test database: {e}")));
        drop(client);
        let _ = conn_task.await;
        created?;
        let mut pg = base.clone();
        pg.dbname(&db);
        let store = Self::from_pg_config(pg, None, TEST_POOL_SIZE, TEST_REPO_NAME)?;
        store.migrate().await?;
        store.ensure_repo().await?;
        Ok(store)
    }

    /// Number of retried transaction attempts (40001, 40003 on reads, internal conflicts)
    /// since this store was built; for metrics and tests of the retry loop
    /// (storage-semantics 3).
    pub fn txn_retries(&self) -> u64 {
        self.inner.retries.load(Ordering::Relaxed)
    }

    /// The repository name this store was opened for (`repos.name`).
    pub fn repo_name(&self) -> &str {
        &self.inner.repo_name
    }

    /// The database this store connects to (e.g. the `scion_test_<rand>` database created by
    /// [`Self::connect_fresh_for_tests`]), when the configuration names one.
    pub fn database(&self) -> Option<&str> {
        self.inner.database.as_deref()
    }

    fn from_pg_config(
        mut pg: tokio_postgres::Config,
        tls: Option<Arc<rustls::ClientConfig>>,
        max_connections: usize,
        repo_name: &str,
    ) -> Result<Self> {
        if pg.get_connect_timeout().is_none() {
            pg.connect_timeout(Duration::from_secs(10));
        }
        let database = pg.get_dbname().map(str::to_owned);
        let mcfg = ManagerConfig {
            recycling_method: RecyclingMethod::Fast,
        };
        let mgr = match tls {
            Some(t) => Manager::from_config(
                pg,
                tokio_postgres_rustls::MakeRustlsConnect::new((*t).clone()),
                mcfg,
            ),
            None => Manager::from_config(pg, NoTls, mcfg),
        };
        let pool = Pool::builder(mgr)
            .max_size(max_connections.max(1))
            .runtime(Runtime::Tokio1)
            .wait_timeout(Some(Duration::from_secs(60)))
            .create_timeout(Some(Duration::from_secs(15)))
            .recycle_timeout(Some(Duration::from_secs(5)))
            .build()
            .map_err(|e| unavailable(format!("metadata connection pool: {e}")))?;
        Ok(CrdbMetaStore {
            inner: Arc::new(Inner {
                pool,
                repo_id: compute_repo_id(repo_name),
                repo_name: repo_name.to_owned(),
                database,
                retries: AtomicU64::new(0),
            }),
        })
    }

    async fn ensure_repo(&self) -> Result<()> {
        let rid = self.rid();
        let name = self.inner.repo_name.as_str();
        self.txn("ensure_repo", false, |mut c| async move {
            let tx = begin(&mut c).await?;
            execute(
                &tx,
                "INSERT INTO repos (repo_id, name) VALUES ($1, $2) ON CONFLICT DO NOTHING",
                &[&rid, &name],
            )
            .await?;
            tx.commit().await?;
            Ok(())
        })
        .await
    }

    fn rid(&self) -> i64 {
        self.inner.repo_id.0
    }

    /// One attempt loop: `f` runs one whole transaction on a fresh pooled connection; retryable
    /// failures are retried with full-jitter backoff up to [`MAX_ATTEMPTS`] attempts.
    async fn run<T, F, Fut>(
        &self,
        op: &'static str,
        read_only: bool,
        f: F,
    ) -> std::result::Result<T, TxErr>
    where
        F: Fn(Object) -> Fut + Send + Sync,
        Fut: Future<Output = std::result::Result<T, TxErr>> + Send,
        T: Send,
    {
        let mut attempt: u32 = 0;
        loop {
            attempt += 1;
            let out = match self.inner.pool.get().await {
                Ok(c) => f(c).await,
                Err(e) => Err(TxErr::Pool(e)),
            };
            match out {
                Ok(v) => return Ok(v),
                Err(e) if e.retryable(read_only) => {
                    self.inner.retries.fetch_add(1, Ordering::Relaxed);
                    if attempt >= MAX_ATTEMPTS {
                        tracing::warn!(
                            target: "scion::store::crdb",
                            op,
                            attempts = attempt,
                            "transaction retries exhausted"
                        );
                        return Err(TxErr::Scion(ScionError::unavailable(
                            subcode::TXN_RETRY,
                            format!("{op}: transaction retries exhausted after {attempt} attempts"),
                        )));
                    }
                    tracing::debug!(
                        target: "scion::store::crdb",
                        op,
                        attempt,
                        error = ?e,
                        "retrying transaction"
                    );
                    tokio::time::sleep(backoff(attempt)).await;
                }
                Err(e) => return Err(e),
            }
        }
    }

    /// [`Self::run`] with the error mapped to the public error model.
    async fn txn<T, F, Fut>(&self, op: &'static str, read_only: bool, f: F) -> Result<T>
    where
        F: Fn(Object) -> Fut + Send + Sync,
        Fut: Future<Output = std::result::Result<T, TxErr>> + Send,
        T: Send,
    {
        self.run(op, read_only, f)
            .await
            .map_err(|e| e.into_scion(op))
    }

    /// A read-only single-statement query: a strong read in one SERIALIZABLE transaction, or a
    /// follower read for [`Consistency::Stale`]. A follower read that predates the schema
    /// (fresh database) falls back to the strong read.
    async fn read_rows(
        &self,
        op: &'static str,
        c: Consistency,
        sql: (&'static str, &'static str),
        params: Params<'_>,
    ) -> Result<Vec<Row>> {
        if c == Consistency::Stale {
            let stale = sql.1;
            let r = self
                .run(op, true, |conn| async move {
                    Ok(query_implicit(&conn, stale, params).await?)
                })
                .await;
            match r {
                Ok(rows) => return Ok(rows),
                Err(TxErr::Db(e)) if is_schema_too_new(&e) => {
                    tracing::debug!(
                        target: "scion::store::crdb",
                        op,
                        "follower read predates the schema; reading strongly"
                    );
                }
                Err(e) => return Err(e.into_scion(op)),
            }
        }
        self.read_strong(op, sql.0, params).await
    }

    /// One strong single-statement read in its own SERIALIZABLE transaction.
    async fn read_strong(
        &self,
        op: &'static str,
        sql: &'static str,
        params: Params<'_>,
    ) -> Result<Vec<Row>> {
        self.txn(op, true, |mut conn| async move {
            let tx = begin(&mut conn).await?;
            let rows = query(&tx, sql, params).await?;
            tx.commit().await?;
            Ok(rows)
        })
        .await
    }
}

#[async_trait]
impl MetaStore for CrdbMetaStore {
    fn repo_id(&self) -> RepoId {
        self.inner.repo_id
    }

    async fn ping(&self) -> Result<()> {
        self.txn("ping", true, |c| async move {
            c.simple_query("SELECT 1").await?;
            Ok(())
        })
        .await
    }

    async fn get_objects(
        &self,
        ids: &[ObjectId],
        c: Consistency,
    ) -> Result<Vec<Option<ObjectRow>>> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let rid = self.rid();
        let keys = ids_bytes(ids);
        let params: Params = &[&rid, &keys];
        let rows = self
            .read_rows("get_objects", c, GET_OBJECTS, params)
            .await?;
        let mut by_id = BTreeMap::new();
        for r in &rows {
            let o = object_row(r)?;
            by_id.insert(o.id, o);
        }
        Ok(ids.iter().map(|i| by_id.get(i).cloned()).collect())
    }

    async fn insert_object(&self, row: ObjectRow) -> Result<InsertOutcome> {
        let rid = self.rid();
        let id = row.id.as_bytes().as_slice();
        let kind = i16::from(row.kind.byte());
        let size = to_i64(row.size, "size")?;
        let stored = to_i64(row.stored_size, "stored_size")?;
        let inline = row.inline_data.as_deref();
        let state = object_state_str(row.state);
        let at = row.created_at;
        self.txn("insert_object", false, |mut c| async move {
            let tx = begin(&mut c).await?;
            let n = execute(
                &tx,
                "INSERT INTO objects (repo_id, object_id, kind, size, stored_size, inline_data, \
                 state, created_at) VALUES ($1, $2, $3, $4, $5, $6, $7, $8) \
                 ON CONFLICT (repo_id, object_id) DO NOTHING",
                &[&rid, &id, &kind, &size, &stored, &inline, &state, &at],
            )
            .await?;
            tx.commit().await?;
            Ok(if n == 1 {
                InsertOutcome::Inserted
            } else {
                InsertOutcome::AlreadyExists
            })
        })
        .await
    }

    async fn record_upload(&self, id: &ObjectId, principal: &str, at: SystemTime) -> Result<()> {
        let rid = self.rid();
        let id = id.as_bytes().as_slice();
        self.txn("record_upload", false, |mut c| async move {
            let tx = begin(&mut c).await?;
            execute(
                &tx,
                "INSERT INTO object_uploads (repo_id, object_id, principal, at) \
                 VALUES ($1, $2, $3, $4) ON CONFLICT DO NOTHING",
                &[&rid, &id, &principal, &at],
            )
            .await?;
            tx.commit().await?;
            Ok(())
        })
        .await
    }

    async fn has_upload(&self, id: &ObjectId, principal: &str) -> Result<bool> {
        let rid = self.rid();
        let id = id.as_bytes().as_slice();
        let rows = self
            .read_strong(
                "has_upload",
                "SELECT 1 FROM object_uploads WHERE repo_id = $1 AND object_id = $2 \
                 AND principal = $3",
                &[&rid, &id, &principal],
            )
            .await?;
        Ok(!rows.is_empty())
    }

    async fn object_paths(&self, id: &ObjectId, limit: usize) -> Result<Vec<ObjectPathRow>> {
        let rid = self.rid();
        let idb = id.as_bytes().as_slice();
        let lim = codec::limit(limit);
        let rows = self
            .read_strong(
                "object_paths",
                "SELECT object_id, path, first_changeset, at FROM object_paths \
                 WHERE repo_id = $1 AND object_id = $2 ORDER BY path LIMIT $3",
                &[&rid, &idb, &lim],
            )
            .await?;
        rows.iter()
            .map(|r| {
                Ok(ObjectPathRow {
                    object: id_of(&col::<Vec<u8>>(r, 0)?)?,
                    path: repo_path_of(&col::<Vec<u8>>(r, 1)?)?,
                    first_changeset: id_of(&col::<Vec<u8>>(r, 2)?)?,
                    at: col(r, 3)?,
                })
            })
            .collect()
    }

    async fn get_changesets(
        &self,
        ids: &[ObjectId],
        c: Consistency,
    ) -> Result<Vec<Option<ChangesetRow>>> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let rid = self.rid();
        let keys = ids_bytes(ids);
        let params: Params = &[&rid, &keys];
        let rows = self
            .read_rows("get_changesets", c, GET_CHANGESETS, params)
            .await?;
        let mut by_id = BTreeMap::new();
        for r in &rows {
            let cs = changeset_row(r)?;
            by_id.insert(cs.id, cs);
        }
        Ok(ids.iter().map(|i| by_id.get(i).cloned()).collect())
    }

    async fn changesets_by_prefix(&self, hex_prefix: &str, limit: usize) -> Result<Vec<ObjectId>> {
        if !valid_hex_prefix(hex_prefix) {
            return Err(ScionError::invalid(
                subcode::BAD_ID_TEXT,
                "changeset prefix must be at most 64 lowercase hex digits",
            ));
        }
        let (lo, hi) = prefix_range(hex_prefix)
            .ok_or_else(|| ScionError::invalid(subcode::BAD_ID_TEXT, "bad changeset prefix"))?;
        let rid = self.rid();
        let lim = codec::limit(limit);
        let rows = self
            .read_strong(
                "changesets_by_prefix",
                "SELECT changeset_id FROM changesets WHERE repo_id = $1 AND changeset_id >= $2 \
                 AND changeset_id < $3 ORDER BY changeset_id LIMIT $4",
                &[&rid, &lo, &hi, &lim],
            )
            .await?;
        rows.iter().map(|r| id_of(&col::<Vec<u8>>(r, 0)?)).collect()
    }

    async fn children(&self, parent: &ObjectId) -> Result<Vec<ObjectId>> {
        let rid = self.rid();
        let p = parent.as_bytes().as_slice();
        let rows = self
            .read_strong(
                "children",
                "SELECT child_id FROM changeset_children WHERE repo_id = $1 AND parent_id = $2 \
                 ORDER BY child_id",
                &[&rid, &p],
            )
            .await?;
        rows.iter().map(|r| id_of(&col::<Vec<u8>>(r, 0)?)).collect()
    }

    async fn has_root_changeset(&self) -> Result<bool> {
        let rid = self.rid();
        let rows = self
            .read_strong(
                "has_root_changeset",
                "SELECT EXISTS (SELECT 1 FROM changesets WHERE repo_id = $1 AND generation = 1)",
                &[&rid],
            )
            .await?;
        match rows.first() {
            Some(r) => col(r, 0),
            None => Ok(false),
        }
    }

    async fn commit_changeset(&self, b: ChangesetBundle) -> Result<InsertOutcome> {
        let rid = self.rid();
        let row = &b.row;
        let id = row.id.as_bytes().as_slice();
        let encoded = row.encoded.as_ref();
        let generation = to_i64(row.generation, "generation")?;
        let skip = ids_bytes(&row.skip);
        let change_id = row.change_id.0.as_slice();
        let created_by = row.created_by.as_str();
        let created_at = row.created_at;
        let parents = ids_bytes(&b.parents);
        let p_objects: Vec<&[u8]> = b
            .object_paths
            .iter()
            .map(|p| p.object.as_bytes().as_slice())
            .collect();
        let p_paths: Vec<&[u8]> = b.object_paths.iter().map(|p| p.path.as_bytes()).collect();
        let p_firsts: Vec<&[u8]> = b
            .object_paths
            .iter()
            .map(|p| p.first_changeset.as_bytes().as_slice())
            .collect();
        let p_ats: Vec<SystemTime> = b.object_paths.iter().map(|p| p.at).collect();
        let derived = DerivedCols::new(&b.derived);
        let (skip, parents, derived) = (&skip, &parents, &derived);
        let (p_objects, p_paths, p_firsts, p_ats) = (&p_objects, &p_paths, &p_firsts, &p_ats);
        self.txn("commit_changeset", false, |mut c| async move {
            let tx = begin(&mut c).await?;
            let n = execute(
                &tx,
                "INSERT INTO changesets (repo_id, changeset_id, encoded, generation, skip, \
                 change_id, created_by, created_at) VALUES ($1, $2, $3, $4, $5, $6, $7, $8) \
                 ON CONFLICT (repo_id, changeset_id) DO NOTHING",
                &[
                    &rid,
                    &id,
                    &encoded,
                    &generation,
                    skip,
                    &change_id,
                    &created_by,
                    &created_at,
                ],
            )
            .await?;
            if !parents.is_empty() {
                execute(
                    &tx,
                    "INSERT INTO changeset_children (repo_id, parent_id, child_id) \
                     SELECT $1, u.p, $3 FROM UNNEST($2::BYTES[]) AS u(p) ON CONFLICT DO NOTHING",
                    &[&rid, parents, &id],
                )
                .await?;
            }
            let mut start = 0;
            while start < p_objects.len() {
                let end = (start + BATCH_ROWS).min(p_objects.len());
                let (o, p, f, a) = (
                    &p_objects[start..end],
                    &p_paths[start..end],
                    &p_firsts[start..end],
                    &p_ats[start..end],
                );
                execute(
                    &tx,
                    "INSERT INTO object_paths (repo_id, object_id, path, first_changeset, at) \
                     SELECT $1, u.o, u.p, u.f, u.a FROM UNNEST($2::BYTES[], $3::BYTES[], \
                     $4::BYTES[], $5::TIMESTAMPTZ[]) AS u(o, p, f, a) ON CONFLICT DO NOTHING",
                    &[&rid, &o, &p, &f, &a],
                )
                .await?;
                start = end;
            }
            insert_derived(&tx, rid, derived).await?;
            tx.commit().await?;
            Ok(if n == 1 {
                InsertOutcome::Inserted
            } else {
                InsertOutcome::AlreadyExists
            })
        })
        .await
    }

    async fn get_ref(&self, name: &RefName) -> Result<Option<RefRow>> {
        let rid = self.rid();
        let n = name.as_str();
        let rows = self.read_strong("get_ref", SELECT_REF, &[&rid, &n]).await?;
        rows.first().map(ref_row).transpose()
    }

    async fn list_refs(&self) -> Result<Vec<RefRow>> {
        let rid = self.rid();
        let rows = self
            .read_strong(
                "list_refs",
                "SELECT name, target, version, updated_at FROM refs WHERE repo_id = $1 \
                 ORDER BY name",
                &[&rid],
            )
            .await?;
        rows.iter().map(ref_row).collect()
    }

    async fn cas_ref(&self, cas: RefCas) -> Result<CasOutcome> {
        if cas.expect == RefExpect::Absent && cas.new_target.is_none() {
            return Err(ScionError::new(
                ErrorCode::InvalidArgument,
                "cas_ref: cannot delete a ref expected to be absent",
            ));
        }
        let mut seen = BTreeSet::new();
        for e in &cas.landed {
            if !seen.insert(e.change_id) {
                return Err(ScionError::new(
                    ErrorCode::InvalidArgument,
                    format!(
                        "cas_ref: duplicate landed change id {}",
                        e.change_id.to_hex()
                    ),
                ));
            }
        }
        let rid = self.rid();
        let name = cas.name.as_str();
        let landed_bytes = encode_landed(&cas.landed);
        let l_change: Vec<&[u8]> = cas
            .landed
            .iter()
            .map(|e| e.change_id.0.as_slice())
            .collect();
        let l_source = cas
            .landed
            .iter()
            .map(|e| e.source.as_bytes().as_slice())
            .collect::<Vec<_>>();
        let l_result = cas
            .landed
            .iter()
            .map(|e| e.result.as_bytes().as_slice())
            .collect::<Vec<_>>();
        let (cas_ref, landed_bytes) = (&cas, &landed_bytes);
        let (l_change, l_source, l_result) = (&l_change, &l_source, &l_result);
        let out = self
            .run("cas_ref", false, |mut c| async move {
                let cas = cas_ref;
                let tx = begin(&mut c).await?;
                let cur = query(&tx, SELECT_REF_FOR_UPDATE, &[&rid, &name]).await?;
                let current = cur.first().map(ref_row).transpose()?;
                let matches = match (&cas.expect, &current) {
                    (RefExpect::Absent, None) => true,
                    (RefExpect::Version(v), Some(r)) => r.version == *v,
                    _ => false,
                };
                if !matches {
                    tx.commit().await?;
                    return Ok(CasOutcome::VersionMismatch { current });
                }
                let new_version = match &cas.expect {
                    RefExpect::Version(v) => v.checked_add(1),
                    RefExpect::Absent => {
                        let r = query(
                            &tx,
                            "SELECT max(version) FROM ref_log WHERE repo_id = $1 AND name = $2",
                            &[&rid, &name],
                        )
                        .await?;
                        let max: Option<i64> = match r.first() {
                            Some(row) => col(row, 0)?,
                            None => None,
                        };
                        match max {
                            None => Some(1),
                            Some(m) => to_u64(m, "ref_log version")?.checked_add(1),
                        }
                    }
                }
                .ok_or_else(|| {
                    ScionError::new(ErrorCode::InvalidArgument, "ref version overflow")
                })?;
                let nv = to_i64(new_version, "ref version")?;
                let at = cas.at;
                match (&cas.expect, &cas.new_target) {
                    (RefExpect::Absent, Some(t)) => {
                        let t = t.as_bytes().as_slice();
                        let n = execute(
                            &tx,
                            "INSERT INTO refs (repo_id, name, target, version, updated_at) \
                             VALUES ($1, $2, $3, $4, $5) ON CONFLICT DO NOTHING",
                            &[&rid, &name, &t, &nv, &at],
                        )
                        .await?;
                        if n != 1 {
                            return Err(TxErr::Conflict("refs insert"));
                        }
                    }
                    (RefExpect::Version(v), Some(t)) => {
                        let t = t.as_bytes().as_slice();
                        let old = to_i64(*v, "ref version")?;
                        let n = execute(
                            &tx,
                            "UPDATE refs SET target = $3, version = $4, updated_at = $5 \
                             WHERE repo_id = $1 AND name = $2 AND version = $6",
                            &[&rid, &name, &t, &nv, &at, &old],
                        )
                        .await?;
                        if n != 1 {
                            return Err(TxErr::Conflict("refs update"));
                        }
                    }
                    (RefExpect::Version(v), None) => {
                        let old = to_i64(*v, "ref version")?;
                        let n = execute(
                            &tx,
                            "DELETE FROM refs WHERE repo_id = $1 AND name = $2 AND version = $3",
                            &[&rid, &name, &old],
                        )
                        .await?;
                        if n != 1 {
                            return Err(TxErr::Conflict("refs delete"));
                        }
                    }
                    (RefExpect::Absent, None) => {
                        return Err(TxErr::Scion(ScionError::new(
                            ErrorCode::InvalidArgument,
                            "cas_ref: cannot delete a ref expected to be absent",
                        )));
                    }
                }
                let old_target = current.as_ref().map(|r| r.target.as_bytes().to_vec());
                let new_target = cas.new_target.map(|t| t.as_bytes().to_vec());
                let principal = cas.principal.as_str();
                let lb = landed_bytes.as_slice();
                execute(
                    &tx,
                    "INSERT INTO ref_log (repo_id, name, version, old_target, new_target, \
                     principal, at, landed) VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
                    &[
                        &rid,
                        &name,
                        &nv,
                        &old_target,
                        &new_target,
                        &principal,
                        &at,
                        &lb,
                    ],
                )
                .await?;
                if !l_change.is_empty() {
                    execute(
                        &tx,
                        "INSERT INTO landed (repo_id, ref, change_id, version, source_changeset, \
                         result_changeset) SELECT $1, $2, u.c, $3, u.s, u.r FROM \
                         UNNEST($4::BYTES[], $5::BYTES[], $6::BYTES[]) AS u(c, s, r)",
                        &[&rid, &name, &nv, l_change, l_source, l_result],
                    )
                    .await
                    .map_err(|e| {
                        if is_unique_violation(&e) {
                            TxErr::LandedUnique
                        } else {
                            TxErr::Db(e)
                        }
                    })?;
                }
                if let Some(a) = &cas.audit {
                    insert_audit(&tx, rid, a).await?;
                }
                tx.commit().await?;
                Ok(CasOutcome::Committed(cas.new_target.map(|target| RefRow {
                    name: cas.name.clone(),
                    target,
                    version: new_version,
                    updated_at: at,
                })))
            })
            .await;
        match out {
            Ok(o) => Ok(o),
            Err(TxErr::LandedUnique) => {
                // The whole transaction rolled back; report the conflicting (immutable) row,
                // read strongly after the rollback (storage-semantics 4.3 step 6).
                for e in &cas.landed {
                    if let Some(existing) = self.get_landed(&cas.name, &e.change_id).await? {
                        return Ok(CasOutcome::LandedConflict { existing });
                    }
                }
                Err(TxErr::LandedUnique.into_scion("cas_ref"))
            }
            Err(e) => Err(e.into_scion("cas_ref")),
        }
    }

    async fn ref_log(
        &self,
        name: &RefName,
        from_version: u64,
        limit: usize,
    ) -> Result<Vec<RefLogRow>> {
        let rid = self.rid();
        let n = name.as_str();
        let from = bound(from_version);
        let lim = codec::limit(limit);
        let rows = self
            .read_strong(
                "ref_log",
                "SELECT name, version, old_target, new_target, principal, at, landed FROM ref_log \
                 WHERE repo_id = $1 AND name = $2 AND version >= $3 ORDER BY version LIMIT $4",
                &[&rid, &n, &from, &lim],
            )
            .await?;
        rows.iter().map(ref_log_row).collect()
    }

    async fn get_landed(&self, name: &RefName, change_id: &ChangeId) -> Result<Option<LandedRow>> {
        let rid = self.rid();
        let n = name.as_str();
        let cid = change_id.0.as_slice();
        let rows = self
            .read_strong("get_landed", SELECT_LANDED, &[&rid, &n, &cid])
            .await?;
        rows.first().map(landed_row).transpose()
    }

    async fn get_derived(&self, k: &DerivedKey, c: Consistency) -> Result<Option<DerivedRow>> {
        let rid = self.rid();
        let ty = k.ty.as_str();
        let ver = i64::from(k.version);
        let key = k.key.as_ref();
        let params: Params = &[&rid, &ty, &ver, &key];
        let rows = self
            .read_rows("get_derived", c, GET_DERIVED, params)
            .await?;
        rows.first().map(|r| derived_row(k, r)).transpose()
    }

    async fn put_derived(&self, rows: Vec<DerivedRow>) -> Result<()> {
        if rows.is_empty() {
            return Ok(());
        }
        let rid = self.rid();
        let cols = DerivedCols::new(&rows);
        let cols = &cols;
        self.txn("put_derived", false, |mut c| async move {
            let tx = begin(&mut c).await?;
            insert_derived(&tx, rid, cols).await?;
            tx.commit().await?;
            Ok(())
        })
        .await
    }

    async fn delete_derived_by_source(&self, types: &[String], source: &ObjectId) -> Result<u64> {
        if types.is_empty() {
            return Ok(0);
        }
        let rid = self.rid();
        let src = source.as_bytes().as_slice();
        self.txn("delete_derived_by_source", false, |mut c| async move {
            let tx = begin(&mut c).await?;
            let n = execute(
                &tx,
                "DELETE FROM derived WHERE repo_id = $1 AND source_object = $2 \
                 AND type = ANY($3)",
                &[&rid, &src, &types],
            )
            .await?;
            tx.commit().await?;
            Ok(n)
        })
        .await
    }

    async fn try_lease(
        &self,
        k: &DerivedKey,
        holder: &str,
        ttl: Duration,
        now: SystemTime,
    ) -> Result<bool> {
        let expires = now
            .checked_add(ttl)
            .ok_or_else(|| ScionError::new(ErrorCode::InvalidArgument, "lease ttl overflow"))?;
        let rid = self.rid();
        let ty = k.ty.as_str();
        let ver = i64::from(k.version);
        let key = k.key.as_ref();
        self.txn("try_lease", false, |mut c| async move {
            let tx = begin(&mut c).await?;
            let rows = query(
                &tx,
                "SELECT holder, expires_at FROM derive_leases WHERE repo_id = $1 AND type = $2 \
                 AND type_version = $3 AND key = $4 FOR UPDATE",
                &[&rid, &ty, &ver, &key],
            )
            .await?;
            if let Some(r) = rows.first() {
                let h: String = col(r, 0)?;
                let exp: SystemTime = col(r, 1)?;
                if h != holder && exp > now {
                    tx.commit().await?;
                    return Ok(false);
                }
            }
            execute(
                &tx,
                "UPSERT INTO derive_leases (repo_id, type, type_version, key, holder, expires_at) \
                 VALUES ($1, $2, $3, $4, $5, $6)",
                &[&rid, &ty, &ver, &key, &holder, &expires],
            )
            .await?;
            tx.commit().await?;
            Ok(true)
        })
        .await
    }

    async fn release_lease(&self, k: &DerivedKey, holder: &str) -> Result<()> {
        let rid = self.rid();
        let ty = k.ty.as_str();
        let ver = i64::from(k.version);
        let key = k.key.as_ref();
        self.txn("release_lease", false, |mut c| async move {
            let tx = begin(&mut c).await?;
            execute(
                &tx,
                "DELETE FROM derive_leases WHERE repo_id = $1 AND type = $2 AND type_version = $3 \
                 AND key = $4 AND holder = $5",
                &[&rid, &ty, &ver, &key, &holder],
            )
            .await?;
            tx.commit().await?;
            Ok(())
        })
        .await
    }

    async fn redact(&self, t: RedactTxn) -> Result<u64> {
        let rid = self.rid();
        let unique = dedup_ids(&t.ids);
        let unique_b = ids_bytes(&unique);
        let (t, unique, unique_b) = (&t, &unique, &unique_b);
        self.txn("redact", false, |mut c| async move {
            let tx = begin(&mut c).await?;
            // Step order matters for contention: the epoch row is locked first.
            let epoch = locked_epoch(&tx, rid).await? + 1;
            let found = query(
                &tx,
                "SELECT object_id FROM objects WHERE repo_id = $1 AND object_id = ANY($2)",
                &[&rid, unique_b],
            )
            .await?;
            let mut present = BTreeSet::new();
            for r in &found {
                present.insert(id_of(&col::<Vec<u8>>(r, 0)?)?);
            }
            if let Some(missing) = unique.iter().find(|id| !present.contains(*id)) {
                return Err(TxErr::Scion(ScionError::not_found(format!(
                    "cannot redact unknown object {missing}"
                ))));
            }
            write_epoch(&tx, rid, epoch).await?;
            if !unique.is_empty() {
                let e = to_i64(epoch, "redaction epoch")?;
                execute(
                    &tx,
                    "INSERT INTO redactions (repo_id, object_id, state, reason, actor, created_at, \
                     purge_after, epoch) SELECT $1, u.id, 'quarantined', $3, $4, $5, $6, $7 FROM \
                     UNNEST($2::BYTES[]) AS u(id) ON CONFLICT (repo_id, object_id) DO UPDATE SET \
                     reason = excluded.reason, actor = excluded.actor, \
                     purge_after = excluded.purge_after, epoch = excluded.epoch \
                     WHERE redactions.state = 'quarantined'",
                    &[
                        &rid,
                        unique_b,
                        &t.reason,
                        &t.actor,
                        &t.at,
                        &t.purge_after,
                        &e,
                    ],
                )
                .await?;
                execute(
                    &tx,
                    "UPDATE objects SET state = 'quarantined' WHERE repo_id = $1 \
                     AND object_id = ANY($2) AND state != 'purged'",
                    &[&rid, unique_b],
                )
                .await?;
            }
            for id in &t.ids {
                let a = AuditRow {
                    at: t.at,
                    actor: t.actor.clone(),
                    action: "redact".to_owned(),
                    target: id.to_text(),
                    reason: t.reason.clone(),
                    detail: String::new(),
                };
                insert_audit(&tx, rid, &a).await?;
            }
            tx.commit().await?;
            Ok(epoch)
        })
        .await
    }

    async fn unredact(&self, ids: &[ObjectId], audit: AuditRow) -> Result<u64> {
        let rid = self.rid();
        let unique = dedup_ids(ids);
        let unique_b = ids_bytes(&unique);
        let (audit, unique, unique_b) = (&audit, &unique, &unique_b);
        self.txn("unredact", false, |mut c| async move {
            let tx = begin(&mut c).await?;
            let epoch = locked_epoch(&tx, rid).await? + 1;
            let found = query(
                &tx,
                "SELECT object_id FROM redactions WHERE repo_id = $1 AND object_id = ANY($2) \
                 AND state = 'quarantined'",
                &[&rid, unique_b],
            )
            .await?;
            let mut quarantined = BTreeSet::new();
            for r in &found {
                quarantined.insert(id_of(&col::<Vec<u8>>(r, 0)?)?);
            }
            if let Some(id) = unique.iter().find(|id| !quarantined.contains(*id)) {
                return Err(TxErr::Scion(ScionError::invalid(
                    subcode::NOT_QUARANTINED,
                    format!("object {id} is not quarantined"),
                )));
            }
            write_epoch(&tx, rid, epoch).await?;
            if !unique.is_empty() {
                execute(
                    &tx,
                    "DELETE FROM redactions WHERE repo_id = $1 AND object_id = ANY($2)",
                    &[&rid, unique_b],
                )
                .await?;
                execute(
                    &tx,
                    "UPDATE objects SET state = 'live' WHERE repo_id = $1 AND object_id = ANY($2)",
                    &[&rid, unique_b],
                )
                .await?;
            }
            insert_audit(&tx, rid, audit).await?;
            tx.commit().await?;
            Ok(epoch)
        })
        .await
    }

    async fn mark_purged(&self, ids: &[ObjectId], audit: AuditRow) -> Result<()> {
        let rid = self.rid();
        let unique = dedup_ids(ids);
        let unique_b = ids_bytes(&unique);
        let (audit, unique_b) = (&audit, &unique_b);
        self.txn("mark_purged", false, |mut c| async move {
            let tx = begin(&mut c).await?;
            if !unique_b.is_empty() {
                execute(
                    &tx,
                    "UPDATE redactions SET state = 'purged' WHERE repo_id = $1 \
                     AND object_id = ANY($2)",
                    &[&rid, unique_b],
                )
                .await?;
                execute(
                    &tx,
                    "UPDATE objects SET state = 'purged', inline_data = NULL WHERE repo_id = $1 \
                     AND object_id = ANY($2)",
                    &[&rid, unique_b],
                )
                .await?;
            }
            insert_audit(&tx, rid, audit).await?;
            tx.commit().await?;
            Ok(())
        })
        .await
    }

    async fn redaction_epoch(&self) -> Result<u64> {
        let rid = self.rid();
        let rows = self
            .read_strong(
                "redaction_epoch",
                "SELECT value FROM counters WHERE repo_id = $1 AND name = 'redaction_epoch'",
                &[&rid],
            )
            .await?;
        match rows.first() {
            Some(r) => to_u64(col(r, 0)?, "redaction epoch"),
            None => Ok(0),
        }
    }

    async fn list_redactions(&self, since_epoch: u64) -> Result<(Vec<RedactionRow>, u64)> {
        let rid = self.rid();
        let since = bound(since_epoch);
        let (rows, epoch) = self
            .txn("list_redactions", true, |mut c| async move {
                let tx = begin(&mut c).await?;
                let rows = query(
                    &tx,
                    "SELECT object_id, state, reason, actor, created_at, purge_after, epoch \
                     FROM redactions WHERE repo_id = $1 AND epoch > $2 \
                     ORDER BY epoch, object_id",
                    &[&rid, &since],
                )
                .await?;
                let ep = query(
                    &tx,
                    "SELECT value FROM counters WHERE repo_id = $1 AND name = 'redaction_epoch'",
                    &[&rid],
                )
                .await?;
                tx.commit().await?;
                Ok((rows, ep))
            })
            .await?;
        let current = match epoch.first() {
            Some(r) => to_u64(col(r, 0)?, "redaction epoch")?,
            None => 0,
        };
        let rows = rows.iter().map(redaction_row).collect::<Result<Vec<_>>>()?;
        Ok((rows, current))
    }

    async fn due_purges(&self, now: SystemTime, limit: usize) -> Result<Vec<ObjectId>> {
        let rid = self.rid();
        let lim = codec::limit(limit);
        let rows = self
            .read_strong(
                "due_purges",
                "SELECT object_id FROM redactions WHERE repo_id = $1 AND state = 'quarantined' \
                 AND purge_after <= $2 ORDER BY purge_after, object_id LIMIT $3",
                &[&rid, &now, &lim],
            )
            .await?;
        rows.iter().map(|r| id_of(&col::<Vec<u8>>(r, 0)?)).collect()
    }

    async fn append_audit(&self, row: AuditRow) -> Result<()> {
        let rid = self.rid();
        let row = &row;
        self.txn("append_audit", false, |mut c| async move {
            let tx = begin(&mut c).await?;
            insert_audit(&tx, rid, row).await?;
            tx.commit().await?;
            Ok(())
        })
        .await
    }

    async fn audit_log(&self, limit: usize) -> Result<Vec<AuditRow>> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let rid = self.rid();
        let lim = codec::limit(limit);
        let rows = self
            .read_strong(
                "audit_log",
                "SELECT at, actor, action, target, reason, detail FROM audit_log \
                 WHERE repo_id = $1 ORDER BY seq DESC LIMIT $2",
                &[&rid, &lim],
            )
            .await?;
        let mut out = rows.iter().map(audit_row).collect::<Result<Vec<_>>>()?;
        out.reverse();
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repo_id_matches_the_in_memory_store() {
        let mem = MemMetaStore::new(MemOptions {
            repo_name: "some-repo".to_owned(),
            ..MemOptions::default()
        });
        assert_eq!(compute_repo_id("some-repo"), mem.repo_id());
        assert!(compute_repo_id("x").0 >= 0);
    }

    #[test]
    fn stale_variants_differ_only_by_the_follower_clause() {
        for (strong, stale) in [GET_OBJECTS, GET_CHANGESETS, GET_DERIVED] {
            assert!(!strong.contains("AS OF SYSTEM TIME"));
            assert_eq!(
                stale.replace(" AS OF SYSTEM TIME follower_read_timestamp()", ""),
                strong
            );
        }
    }

    #[test]
    fn dedup_sorts_and_removes_duplicates() {
        let a = ObjectId::hash_blob(b"a");
        let b = ObjectId::hash_blob(b"b");
        let d = dedup_ids(&[b, a, b]);
        assert_eq!(d.len(), 2);
        assert!(d[0] < d[1]);
    }
}
