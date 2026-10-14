//! CockroachDB backend tests (PLAN 3.3; SPEC 5.2-5.4, 16.1 class 1, 19.3 on a real backend).
//!
//! Every `external_*` test is gated on `SCION_TEST_CRDB_URL`: when it is unset the test prints
//! `SKIP: ...` and passes. Each test works in its own fresh database
//! (`CrdbMetaStore::connect_fresh_for_tests`). The remaining tests are hermetic.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test crate

use bytes::Bytes;
use scion_core::{ErrorCode, FaultAction, Kind, ObjectId, subcode};
use scion_object::RefName;
use scion_store::{
    Area, BlobStore, CasOutcome, Consistency, DerivedKey, DerivedRow, InsertOutcome, MemBlobStore,
    MetaStore, ObjectRepo, ObjectRepoConfig, ObjectRow, ObjectState, RedactTxn, RefCas, RefExpect,
    Storage, conformance,
};
use scion_store_crdb::{CrdbConfig, CrdbMetaStore, MIGRATIONS, test_url};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio_postgres::NoTls;

/// The gate: `Some(url)` when the external CockroachDB is configured.
fn gate(test: &str) -> Option<String> {
    let url = test_url();
    if url.is_none() {
        println!("SKIP: SCION_TEST_CRDB_URL unset ({test})");
    }
    url
}

async fn fresh(url: &str) -> CrdbMetaStore {
    CrdbMetaStore::connect_fresh_for_tests(url)
        .await
        .expect("fresh test database")
}

fn t(n: u64) -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(1_700_000_000 + n)
}

fn blob_row(content: &[u8]) -> ObjectRow {
    ObjectRow {
        id: ObjectId::hash_blob(content),
        kind: Kind::Blob,
        size: content.len() as u64,
        stored_size: content.len() as u64 + 9,
        inline_data: None,
        state: ObjectState::Live,
        created_at: t(1),
    }
}

fn cs_id(n: u32) -> ObjectId {
    ObjectId::hash_encoded(Kind::Changeset, &n.to_be_bytes())
}

fn cas(name: &RefName, expect: RefExpect, target: ObjectId) -> RefCas {
    RefCas {
        name: name.clone(),
        expect,
        new_target: Some(target),
        principal: "github:alice".to_owned(),
        at: t(10),
        landed: Vec::new(),
        audit: None,
    }
}

/// A raw connection to the database `db` on the server of `base_url`.
async fn raw(base_url: &str, db: Option<&str>) -> tokio_postgres::Client {
    let mut cfg: tokio_postgres::Config = base_url.parse().unwrap();
    if let Some(db) = db {
        cfg.dbname(db);
    }
    let (client, conn) = cfg.connect(NoTls).await.unwrap();
    tokio::spawn(conn);
    client
}

/// The database of a store created by `connect_fresh_for_tests`.
fn current_db(store: &CrdbMetaStore) -> String {
    let db = store.database().expect("test stores name their database");
    assert!(db.starts_with("scion_test_"), "{db}");
    db.to_owned()
}

/// `base_url` with its database replaced by `db`.
fn url_with_db(base_url: &str, db: &str) -> String {
    let (head, query) = match base_url.split_once('?') {
        Some((h, q)) => (h, Some(q)),
        None => (base_url, None),
    };
    let scheme_end = head.find("://").map_or(0, |i| i + 3);
    let path_start = head[scheme_end..]
        .find('/')
        .map_or(head.len(), |i| scheme_end + i);
    let mut out = format!("{}/{db}", &head[..path_start]);
    if let Some(q) = query {
        out.push('?');
        out.push_str(q);
    }
    out
}

#[test]
fn url_with_db_replaces_only_the_database() {
    assert_eq!(
        url_with_db(
            "postgresql://root@localhost:26257/defaultdb?sslmode=disable",
            "x"
        ),
        "postgresql://root@localhost:26257/x?sslmode=disable"
    );
    assert_eq!(
        url_with_db("postgresql://root@localhost:26257", "y"),
        "postgresql://root@localhost:26257/y"
    );
}

// ---------------------------------------------------------------------------------------------
// Hermetic tests (no database needed)
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn crdb_connect_refused_is_store_unavailable() {
    // Nothing listens on port 1 of the loopback interface (SPEC 16.1 class 1).
    let cfg = CrdbConfig {
        url: "postgresql://root@127.0.0.1:1/defaultdb?sslmode=disable&connect_timeout=2".to_owned(),
        max_connections: 2,
        repo_name: "test".to_owned(),
        tls: None,
    };
    let e = CrdbMetaStore::connect(cfg).await.unwrap_err();
    assert_eq!(e.code, ErrorCode::Unavailable, "{e}");
    assert_eq!(e.subcode(), Some(subcode::STORE_UNAVAILABLE), "{e}");
    assert!(e.retryable());
}

#[tokio::test]
async fn crdb_invalid_url_is_invalid_argument() {
    let cfg = CrdbConfig {
        url: "postgresql://root@localhost:notaport/db".to_owned(),
        max_connections: 2,
        repo_name: "test".to_owned(),
        tls: None,
    };
    let e = CrdbMetaStore::connect(cfg).await.unwrap_err();
    assert_eq!(e.code, ErrorCode::InvalidArgument, "{e}");
    let e = CrdbMetaStore::connect_fresh_for_tests("not a url at all ::")
        .await
        .unwrap_err();
    assert_eq!(e.code, ErrorCode::InvalidArgument, "{e}");
}

#[test]
fn migrations_cover_every_table_and_index() {
    let all: String = MIGRATIONS.iter().map(|(_, sql)| *sql).collect();
    for table in [
        "repos",
        "objects",
        "object_uploads",
        "object_paths",
        "changesets",
        "changeset_children",
        "refs",
        "ref_log",
        "landed",
        "derived",
        "derive_leases",
        "redactions",
        "counters",
        "audit_log",
    ] {
        assert!(
            all.contains(&format!("CREATE TABLE IF NOT EXISTS {table} (")),
            "missing table {table}"
        );
    }
    for index in [
        "derived_by_source",
        "redactions_by_epoch",
        "redactions_by_purge_after",
        "changesets_roots",
    ] {
        assert!(
            all.contains(&format!("CREATE INDEX IF NOT EXISTS {index} ")),
            "missing index {index}"
        );
    }
    let mut names: Vec<&str> = MIGRATIONS.iter().map(|(n, _)| *n).collect();
    let n = names.len();
    names.sort_unstable();
    names.dedup();
    assert_eq!(names.len(), n, "migration names are unique");
}

#[tokio::test]
async fn gated_tests_skip_without_the_env_var() {
    // With the gate unset every external test returns early; with it set this is a no-op.
    if test_url().is_none() {
        assert!(gate("gated_tests_skip_without_the_env_var").is_none());
    }
}

// ---------------------------------------------------------------------------------------------
// External tests
// ---------------------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn external_crdb_meta_store_conformance() {
    let Some(url) = gate("external_crdb_meta_store_conformance") else {
        return;
    };
    conformance::meta_store_suite(|| {
        let url = url.clone();
        async move { Arc::new(fresh(&url).await) as Arc<dyn MetaStore> }
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn external_crdb_migrate_is_idempotent() {
    let Some(url) = gate("external_crdb_migrate_is_idempotent") else {
        return;
    };
    let store = fresh(&url).await;
    let row = blob_row(b"survives migrations");
    assert_eq!(
        store.insert_object(row.clone()).await.unwrap(),
        InsertOutcome::Inserted
    );
    store.migrate().await.unwrap();
    store.migrate().await.unwrap();
    let db = current_db(&store);

    // A second server connecting to the same database re-runs the migrations and keeps the
    // single repos row.
    let again = CrdbMetaStore::connect(CrdbConfig {
        url: url_with_db(&url, &db),
        max_connections: 2,
        repo_name: "test".to_owned(),
        tls: None,
    })
    .await
    .unwrap();
    again.migrate().await.unwrap();
    assert_eq!(again.repo_id(), store.repo_id());
    assert_eq!(
        again
            .get_objects(&[row.id], Consistency::Strong)
            .await
            .unwrap(),
        vec![Some(row.clone())]
    );

    let c = raw(&url, Some(&db)).await;
    let repos: i64 = c
        .query_one("SELECT count(*) FROM repos", &[])
        .await
        .unwrap()
        .get(0);
    assert_eq!(repos, 1, "one repos row");
    let tables: Vec<String> = c
        .query(
            "SELECT table_name FROM information_schema.tables WHERE table_schema = 'public' \
             ORDER BY table_name",
            &[],
        )
        .await
        .unwrap()
        .iter()
        .map(|r| r.get(0))
        .collect();
    for want in [
        "audit_log",
        "changeset_children",
        "changesets",
        "counters",
        "derive_leases",
        "derived",
        "landed",
        "object_paths",
        "object_uploads",
        "objects",
        "redactions",
        "ref_log",
        "refs",
        "repos",
    ] {
        assert!(tables.iter().any(|t| t == want), "table {want}: {tables:?}");
    }
    let indexes: Vec<String> = c
        .query(
            "SELECT DISTINCT index_name FROM information_schema.statistics \
             WHERE table_schema = 'public'",
            &[],
        )
        .await
        .unwrap()
        .iter()
        .map(|r| r.get(0))
        .collect();
    for want in [
        "derived_by_source",
        "redactions_by_epoch",
        "redactions_by_purge_after",
        "changesets_roots",
    ] {
        assert!(
            indexes.iter().any(|i| i == want),
            "index {want}: {indexes:?}"
        );
    }
    // Every primary key starts with repo_id (storage-semantics 1.5).
    let pk_first: Vec<(String, String)> = c
        .query(
            "SELECT table_name, column_name FROM information_schema.statistics \
             WHERE table_schema = 'public' AND index_name = table_name || '_pkey' \
             AND seq_in_index = 1",
            &[],
        )
        .await
        .unwrap()
        .iter()
        .map(|r| (r.get(0), r.get(1)))
        .collect();
    assert_eq!(pk_first.len(), 14, "{pk_first:?}");
    assert!(
        pk_first.iter().all(|(_, col)| col == "repo_id"),
        "{pk_first:?}"
    );
}

/// Two conflicting concurrent transactions: a raw transaction T1 holds a write intent on the
/// `redactions` row the store's `redact` will upsert, the store reads the `objects` row and
/// blocks on that intent, then T1 overwrites the `objects` row and commits. The store's later
/// write of `objects` cannot be ordered before T1 any more, so its transaction fails with
/// SQLSTATE 40001 and the client-side retry loop re-runs it to success.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn external_crdb_retries_serialization_failures() {
    let Some(url) = gate("external_crdb_retries_serialization_failures") else {
        return;
    };
    let store = fresh(&url).await;
    let a = blob_row(b"contended object");
    store.insert_object(a.clone()).await.unwrap();
    let db = current_db(&store);
    let rid = store.repo_id().0;
    let id = a.id.as_bytes().to_vec();

    let mut t1_client = raw(&url, Some(&db)).await;
    let t1 = t1_client.transaction().await.unwrap();
    t1.execute(
        "INSERT INTO redactions (repo_id, object_id, state, reason, actor, created_at, \
         purge_after, epoch) VALUES ($1, $2, 'quarantined', 'raw', 'raw', now(), now(), 0)",
        &[&rid, &id],
    )
    .await
    .unwrap();

    let before = store.txn_retries();
    let s = store.clone();
    let oid = a.id;
    let redact = tokio::spawn(async move {
        s.redact(RedactTxn {
            ids: vec![oid],
            reason: "store".to_owned(),
            actor: "github:admin".to_owned(),
            at: t(30),
            purge_after: t(300),
        })
        .await
    });
    // Wait until the store has read `objects` and is blocked on T1's intent: its redactions
    // upsert (which runs after the objects read) is visible as an active cluster query.
    let observer = raw(&url, None).await;
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    loop {
        let blocked: i64 = observer
            .query_one(
                "SELECT count(*) FROM crdb_internal.cluster_queries \
                 WHERE query LIKE 'INSERT INTO redactions%' AND query LIKE '%ON CONFLICT%'",
                &[],
            )
            .await
            .unwrap()
            .get(0);
        if blocked > 0 {
            break;
        }
        assert!(!redact.is_finished(), "redact must block on T1's intent");
        assert!(
            std::time::Instant::now() < deadline,
            "the store's redact never reached the redactions upsert"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(!redact.is_finished(), "redact must be blocked on T1");
    t1.execute(
        "UPDATE objects SET stored_size = stored_size WHERE repo_id = $1 AND object_id = $2",
        &[&rid, &id],
    )
    .await
    .unwrap();
    t1.commit().await.unwrap();

    let epoch = tokio::time::timeout(Duration::from_secs(60), redact)
        .await
        .expect("redact finishes")
        .unwrap()
        .expect("redact succeeds after retrying");
    assert_eq!(epoch, 1);
    assert!(
        store.txn_retries() > before,
        "the store retried a 40001 (retries before {before}, after {})",
        store.txn_retries()
    );
    let (rows, cur) = store.list_redactions(0).await.unwrap();
    assert_eq!(cur, 1);
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0].reason, "store",
        "the retried redact refreshed T1's row"
    );
    assert_eq!(rows[0].epoch, 1);
    let objs = store
        .get_objects(&[a.id], Consistency::Strong)
        .await
        .unwrap();
    assert_eq!(objs[0].as_ref().unwrap().state, ObjectState::Quarantined);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn external_crdb_stale_reads_may_miss_recent_rows() {
    let Some(url) = gate("external_crdb_stale_reads_may_miss_recent_rows") else {
        return;
    };
    let store = fresh(&url).await;
    // Follower reads lag a few seconds; wait until the schema itself (and a probe row) is
    // older than the follower-read timestamp, so stale reads below are real follower reads
    // rather than the strong fallback used while the database is newer than that timestamp.
    let probe = blob_row(b"probe");
    store.insert_object(probe.clone()).await.unwrap();
    let lag_ms: f64 = raw(&url, None)
        .await
        .query_one(
            "SELECT extract(epoch FROM now() - follower_read_timestamp())::FLOAT8 * 1000",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    tokio::time::sleep(Duration::from_millis(lag_ms as u64 + 1500)).await;
    assert_eq!(
        store
            .get_objects(&[probe.id], Consistency::Stale)
            .await
            .unwrap(),
        vec![Some(probe.clone())],
        "a row older than the follower-read lag is visible"
    );

    let row = blob_row(b"just committed");
    store.insert_object(row.clone()).await.unwrap();
    let k = DerivedKey {
        ty: "blame".to_owned(),
        version: 1,
        key: Bytes::from_static(b"recent"),
    };
    let d = DerivedRow {
        key: k.clone(),
        value: Some(Bytes::from_static(b"v")),
        storage_key: None,
        source_object: None,
        at: t(4),
    };
    store.put_derived(vec![d.clone()]).await.unwrap();

    // Strong reads see the rows immediately; stale reads (AS OF SYSTEM TIME
    // follower_read_timestamp()) miss them, but never invent or alter a row.
    assert_eq!(
        store
            .get_objects(&[row.id], Consistency::Strong)
            .await
            .unwrap(),
        vec![Some(row.clone())]
    );
    assert_eq!(
        store.get_derived(&k, Consistency::Strong).await.unwrap(),
        Some(d.clone())
    );
    let stale = store
        .get_objects(&[row.id, probe.id], Consistency::Stale)
        .await
        .unwrap();
    assert_eq!(
        stale[0], None,
        "a just-committed row is not yet visible to a follower read"
    );
    assert_eq!(stale[1], Some(probe.clone()), "older rows are visible");
    assert_eq!(
        store.get_derived(&k, Consistency::Stale).await.unwrap(),
        None,
        "stale derived read misses the recent row"
    );

    // SPEC 5.4 / 19.3: ObjectRepo retries a stale miss strongly before reporting NOT_FOUND,
    // so the just-committed row is found although the follower read above misses it.
    let shared: Arc<dyn MetaStore> = Arc::new(store.clone());
    let (repo, _blobs) = repo_over(shared);
    assert_eq!(repo.stat(&row.id).await.unwrap(), Some(row.clone()));
    let absent = ObjectId::hash_blob(b"never inserted");
    assert_eq!(
        repo.stat_many(&[row.id, absent, probe.id]).await.unwrap(),
        vec![Some(row.clone()), None, Some(probe.clone())]
    );
    assert_eq!(
        store
            .get_objects(&[row.id], Consistency::Stale)
            .await
            .unwrap(),
        vec![None],
        "the follower read still misses the row (the strong retry found it)"
    );

    // Eventually the follower read catches up and returns the identical row.
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    loop {
        let got = store
            .get_objects(&[row.id], Consistency::Stale)
            .await
            .unwrap();
        if let Some(r) = &got[0] {
            assert_eq!(r, &row);
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "stale read never caught up"
        );
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    assert_eq!(
        store.get_derived(&k, Consistency::Stale).await.unwrap(),
        Some(d)
    );
}

async fn concurrent_cas(url: &str, racers: u32) {
    let store = Arc::new(fresh(url).await);
    let main = RefName::main();
    store
        .cas_ref(cas(&main, RefExpect::Absent, cs_id(0)))
        .await
        .unwrap();
    let handles: Vec<_> = (0..racers)
        .map(|i| {
            let s = store.clone();
            let c = cas(&main, RefExpect::Version(1), cs_id(i + 1));
            tokio::spawn(async move { s.cas_ref(c).await })
        })
        .collect();
    let mut winners = Vec::new();
    let mut losers = 0u32;
    for h in handles {
        match h.await.unwrap() {
            Ok(CasOutcome::Committed(Some(r))) => winners.push(r),
            Ok(CasOutcome::VersionMismatch { current: Some(r) }) => {
                assert_eq!(r.version, 2, "losers observe the winner's version");
                losers += 1;
            }
            other => panic!("unexpected outcome {other:?}"),
        }
    }
    assert_eq!(winners.len(), 1, "exactly one winner");
    assert_eq!(losers, racers - 1);
    let w = &winners[0];
    assert_eq!(w.version, 2);
    assert_eq!(store.get_ref(&main).await.unwrap(), Some(w.clone()));
    let log = store.ref_log(&main, 0, 10).await.unwrap();
    assert_eq!(
        log.iter().map(|r| r.version).collect::<Vec<_>>(),
        vec![1, 2]
    );
    assert_eq!(log[1].new_target, Some(w.target));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn external_crdb_concurrent_cas_1000_way_has_one_winner() {
    let Some(url) = gate("external_crdb_concurrent_cas_1000_way_has_one_winner") else {
        return;
    };
    concurrent_cas(&url, 1000).await;
}

fn repo_over(meta: Arc<dyn MetaStore>) -> (ObjectRepo, Arc<MemBlobStore>) {
    let blobs = Arc::new(MemBlobStore::new());
    let storage = Storage::new(meta, blobs.clone(), scion_core::FaultRegistry::new());
    (ObjectRepo::new(storage, ObjectRepoConfig::default()), blobs)
}

/// SPEC 19.3 on CockroachDB: an object storage failure before the bytes are durable leaves no
/// `objects` row.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn external_crdb_object_repo_blob_put_failure_leaves_no_row() {
    let Some(url) = gate("external_crdb_object_repo_blob_put_failure_leaves_no_row") else {
        return;
    };
    let meta = Arc::new(fresh(&url).await);
    let (repo, blobs) = repo_over(meta.clone());
    repo.storage().faults.arm(
        "blob.put.before",
        FaultAction::Error(ErrorCode::Unavailable),
        Some(1),
    );
    let content = Bytes::from_static(b"never durable");
    let id = ObjectId::hash_blob(&content);
    let e = repo.put_blob(content.clone()).await.unwrap_err();
    assert_eq!(e.code, ErrorCode::Unavailable);
    assert!(blobs.keys().is_empty(), "no bytes written");
    assert_eq!(
        meta.get_objects(&[id], Consistency::Strong).await.unwrap(),
        vec![None],
        "no objects row without durable bytes"
    );
    assert_eq!(repo.stat(&id).await.unwrap(), None);
    assert_eq!(repo.get(&id).await.unwrap_err().code, ErrorCode::NotFound);

    // A put whose bytes landed but whose response was lost (blob.put.after FailAfter) leaves an
    // invisible orphan and still no row.
    repo.storage()
        .faults
        .arm("blob.put.after", FaultAction::FailAfter, Some(1));
    let e = repo.put_blob(content.clone()).await.unwrap_err();
    assert_eq!(e.subcode(), Some(subcode::INJECTED));
    assert_eq!(blobs.keys(), vec![repo.key(&id, Area::Live)]);
    assert_eq!(
        meta.get_objects(&[id], Consistency::Strong).await.unwrap(),
        vec![None]
    );
    // The retry completes the upload.
    let (got, outcome) = repo.put_blob(content.clone()).await.unwrap();
    assert_eq!((got, outcome), (id, InsertOutcome::Inserted));
    assert_eq!(repo.get(&id).await.unwrap(), content);
}

/// SPEC 19.3 on CockroachDB: FailAfter on `meta.insert_object` ("committed, response lost")
/// leaves a durable row whose bytes are durable too; the retry reports AlreadyExists.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn external_crdb_object_repo_fail_after_insert_leaves_row_and_bytes() {
    let Some(url) = gate("external_crdb_object_repo_fail_after_insert_leaves_row_and_bytes") else {
        return;
    };
    let meta = Arc::new(fresh(&url).await);
    let (repo, blobs) = repo_over(meta.clone());
    repo.storage()
        .faults
        .arm("meta.insert_object.after", FaultAction::FailAfter, Some(1));
    let content = Bytes::from(vec![7u8; 100_000]);
    let id = ObjectId::hash_blob(&content);
    let e = repo.put_blob(content.clone()).await.unwrap_err();
    assert_eq!(e.code, ErrorCode::Unavailable);
    assert_eq!(e.subcode(), Some(subcode::INJECTED));
    let row = meta.get_objects(&[id], Consistency::Strong).await.unwrap()[0]
        .clone()
        .expect("the row committed");
    assert_eq!(row.state, ObjectState::Live);
    assert_eq!(row.size, content.len() as u64);
    let key = repo.key(&id, Area::Live);
    let stored = blobs.get(&key).await.unwrap().expect("durable bytes");
    assert_eq!(stored.len() as u64, row.stored_size);
    assert_eq!(repo.get(&id).await.unwrap(), content);
    let (got, outcome) = repo.put_blob(content.clone()).await.unwrap();
    assert_eq!((got, outcome), (id, InsertOutcome::AlreadyExists));
}

fn audit(n: u64) -> scion_store::AuditRow {
    scion_store::AuditRow {
        at: t(n),
        actor: "github:admin".to_owned(),
        action: format!("action-{n}"),
        target: format!("target-{n}"),
        reason: "r".to_owned(),
        detail: String::new(),
    }
}

/// storage-semantics 4.6: every audit row gets a new monotonically increasing `seq` from the
/// in-transaction counter (dense and in commit order), including the rows written by `redact`
/// and by `cas_ref`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn external_crdb_audit_seq_is_monotonic() {
    let Some(url) = gate("external_crdb_audit_seq_is_monotonic") else {
        return;
    };
    let store = fresh(&url).await;
    store.append_audit(audit(1)).await.unwrap();
    let a = blob_row(b"audited a");
    let b = blob_row(b"audited b");
    store.insert_object(a.clone()).await.unwrap();
    store.insert_object(b.clone()).await.unwrap();
    store
        .redact(RedactTxn {
            ids: vec![a.id, b.id],
            reason: "leak".to_owned(),
            actor: "github:admin".to_owned(),
            at: t(2),
            purge_after: t(1000),
        })
        .await
        .unwrap();
    let mut c = cas(&RefName::main(), RefExpect::Absent, cs_id(1));
    c.audit = Some(audit(3));
    assert!(matches!(
        store.cas_ref(c).await.unwrap(),
        CasOutcome::Committed(Some(_))
    ));
    // Concurrent appends all get distinct sequence numbers.
    let handles: Vec<_> = (10..30u64)
        .map(|n| {
            let s = store.clone();
            tokio::spawn(async move { s.append_audit(audit(n)).await })
        })
        .collect();
    for h in handles {
        h.await.unwrap().unwrap();
    }
    let db = current_db(&store);
    let seqs: Vec<i64> = raw(&url, Some(&db))
        .await
        .query("SELECT seq FROM audit_log ORDER BY seq", &[])
        .await
        .unwrap()
        .iter()
        .map(|r| r.get(0))
        .collect();
    assert_eq!(seqs, (1..=24).collect::<Vec<i64>>(), "dense, monotonic seq");
    let log = store.audit_log(4).await.unwrap();
    assert_eq!(log.len(), 4);
    let head = store.audit_log(100).await.unwrap();
    assert_eq!(head.len(), 24);
    assert_eq!(head[0].action, "action-1");
    assert_eq!(head[1].action, "redact");
    assert_eq!(head[2].action, "redact");
    assert_eq!(head[3].action, "action-3");
    assert_eq!(
        &head[20..],
        &log[..],
        "audit_log(limit) is the newest rows, oldest first"
    );
}

/// cas_ref checks the expected version before validating the landed list, like the in-memory
/// store: a mismatch wins over a duplicate change id, which is otherwise INVALID_ARGUMENT.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn external_crdb_cas_ref_duplicate_landed_after_version_check() {
    let Some(url) = gate("external_crdb_cas_ref_duplicate_landed_after_version_check") else {
        return;
    };
    let store = fresh(&url).await;
    let main = RefName::main();
    let entry = scion_store::LandedEntry {
        change_id: scion_core::ChangeId([5; 16]),
        source: cs_id(10),
        result: cs_id(11),
    };
    let mut dup = cas(&main, RefExpect::Version(7), cs_id(1));
    dup.landed = vec![entry.clone(), entry.clone()];
    assert_eq!(
        store.cas_ref(dup.clone()).await.unwrap(),
        CasOutcome::VersionMismatch { current: None }
    );
    dup.expect = RefExpect::Absent;
    let e = store.cas_ref(dup).await.unwrap_err();
    assert_eq!(e.code, ErrorCode::InvalidArgument, "{e}");
    assert_eq!(store.get_ref(&main).await.unwrap(), None, "no effects");
    assert!(store.ref_log(&main, 0, 10).await.unwrap().is_empty());
}

/// `has_root_changeset` is served by the `changesets_roots` partial index instead of a scan
/// of every changeset.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn external_crdb_has_root_changeset_uses_the_roots_index() {
    let Some(url) = gate("external_crdb_has_root_changeset_uses_the_roots_index") else {
        return;
    };
    let store = fresh(&url).await;
    assert!(!store.has_root_changeset().await.unwrap());
    let db = current_db(&store);
    let plan: Vec<String> = raw(&url, Some(&db))
        .await
        .query(
            "EXPLAIN SELECT EXISTS (SELECT 1 FROM changesets WHERE repo_id = 1 AND generation = 1)",
            &[],
        )
        .await
        .unwrap()
        .iter()
        .map(|r| r.get(0))
        .collect();
    assert!(
        plan.iter().any(|l| l.contains("changesets_roots")),
        "{plan:#?}"
    );
}
