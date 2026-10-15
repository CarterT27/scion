//! Transaction plumbing: the internal error type, SQLSTATE classification, full-jitter backoff
//! and statement helpers (`docs/contracts/storage-semantics.md` section 3).

use deadpool_postgres::{Object, PoolError};
use rand::Rng;
use scion_core::{ScionError, subcode};
use std::time::Duration;
use tokio_postgres::error::SqlState;
use tokio_postgres::types::ToSql;
use tokio_postgres::{IsolationLevel, Row};

/// At most this many attempts per method (storage-semantics 3).
pub(crate) const MAX_ATTEMPTS: u32 = 10;
/// Full-jitter backoff: attempt `k` sleeps uniformly in `[0, min(CAP, BASE * 2^k))`.
const BACKOFF_BASE: Duration = Duration::from_millis(2);
const BACKOFF_CAP: Duration = Duration::from_millis(250);

/// Statement parameters.
pub(crate) type Params<'a> = &'a [&'a (dyn ToSql + Sync)];

/// Errors inside one transaction attempt.
#[derive(Debug)]
pub(crate) enum TxErr {
    /// A database error (classified by SQLSTATE).
    Db(tokio_postgres::Error),
    /// The pool could not hand out a connection.
    Pool(PoolError),
    /// A semantic error decided by the method (no effects; the transaction rolls back).
    Scion(ScionError),
    /// A unique violation (23505) on `landed` inside `cas_ref` (storage-semantics 4.3 step 6).
    LandedUnique,
    /// A write that a serializable transaction should not have been able to miss (an UPDATE
    /// guarded by the version just read affected no row); retried like 40001.
    Conflict(&'static str),
}

impl From<tokio_postgres::Error> for TxErr {
    fn from(e: tokio_postgres::Error) -> Self {
        TxErr::Db(e)
    }
}

impl From<PoolError> for TxErr {
    fn from(e: PoolError) -> Self {
        TxErr::Pool(e)
    }
}

impl From<ScionError> for TxErr {
    fn from(e: ScionError) -> Self {
        TxErr::Scion(e)
    }
}

/// True for SQLSTATE 23505.
pub(crate) fn is_unique_violation(e: &tokio_postgres::Error) -> bool {
    e.code() == Some(&SqlState::UNIQUE_VIOLATION)
}

/// True when a follower read failed because the schema did not exist yet at the follower-read
/// timestamp (a freshly created database, table or column, e.g. right after migration 0004).
/// The caller falls back to a strong read, which is always a valid answer to a stale read
/// (storage-semantics 1.7).
pub(crate) fn is_schema_too_new(e: &tokio_postgres::Error) -> bool {
    matches!(
        e.code(),
        Some(s) if *s == SqlState::UNDEFINED_TABLE
            || *s == SqlState::INVALID_CATALOG_NAME
            || *s == SqlState::UNDEFINED_COLUMN
    )
}

impl TxErr {
    /// Whether another attempt may succeed: 40001 always, 40003 (ambiguous result) only for
    /// read-only methods, and internal [`TxErr::Conflict`]s.
    pub(crate) fn retryable(&self, read_only: bool) -> bool {
        match self {
            TxErr::Db(e) => match e.code() {
                Some(s) if *s == SqlState::T_R_SERIALIZATION_FAILURE => true,
                Some(s) if *s == SqlState::T_R_STATEMENT_COMPLETION_UNKNOWN => read_only,
                _ => false,
            },
            TxErr::Conflict(_) => true,
            TxErr::Pool(_) | TxErr::Scion(_) | TxErr::LandedUnique => false,
        }
    }

    /// The public error: semantic errors pass through; every infrastructure failure is
    /// UNAVAILABLE/store_unavailable (storage-semantics 1.2, 3).
    pub(crate) fn into_scion(self, op: &str) -> ScionError {
        match self {
            TxErr::Scion(e) => e,
            TxErr::Db(e) => {
                match e.code() {
                    Some(code) => tracing::error!(
                        target: "scion::store::crdb",
                        op,
                        sqlstate = code.code(),
                        error = %e,
                        "unexpected database error"
                    ),
                    None => tracing::warn!(
                        target: "scion::store::crdb",
                        op,
                        error = %e,
                        "database connection error"
                    ),
                }
                ScionError::unavailable(subcode::STORE_UNAVAILABLE, format!("{op}: {e}"))
            }
            TxErr::Pool(e) => {
                tracing::warn!(
                    target: "scion::store::crdb",
                    op,
                    error = %e,
                    "connection pool error"
                );
                ScionError::unavailable(
                    subcode::STORE_UNAVAILABLE,
                    format!("{op}: metadata store unavailable: {e}"),
                )
            }
            TxErr::LandedUnique => {
                tracing::error!(
                    target: "scion::store::crdb",
                    op,
                    "unhandled landed unique violation"
                );
                ScionError::unavailable(
                    subcode::STORE_UNAVAILABLE,
                    format!("{op}: unique violation"),
                )
            }
            TxErr::Conflict(what) => ScionError::unavailable(
                subcode::TXN_RETRY,
                format!("{op}: transaction conflict ({what})"),
            ),
        }
    }
}

/// A corrupt or undecodable stored value: UNAVAILABLE/store_unavailable with an ERROR event.
pub(crate) fn corrupt(what: impl std::fmt::Display) -> ScionError {
    tracing::error!(target: "scion::store::crdb", detail = %what, "corrupt metadata row");
    ScionError::unavailable(
        subcode::STORE_UNAVAILABLE,
        format!("corrupt metadata row: {what}"),
    )
}

/// The full-jitter delay after failed attempt number `attempt` (1-based).
pub(crate) fn backoff(attempt: u32) -> Duration {
    let exp = BACKOFF_BASE.saturating_mul(1u32 << attempt.min(16));
    let ceiling = exp.min(BACKOFF_CAP);
    let micros = u64::try_from(ceiling.as_micros())
        .unwrap_or(u64::MAX)
        .max(1);
    Duration::from_micros(rand::rng().random_range(0..micros))
}

/// `BEGIN ISOLATION LEVEL SERIALIZABLE` on a pooled connection.
pub(crate) async fn begin(
    c: &mut Object,
) -> Result<deadpool_postgres::Transaction<'_>, tokio_postgres::Error> {
    c.build_transaction()
        .isolation_level(IsolationLevel::Serializable)
        .start()
        .await
}

/// Run a (cached) prepared query inside a transaction.
pub(crate) async fn query(
    tx: &deadpool_postgres::Transaction<'_>,
    sql: &str,
    params: Params<'_>,
) -> Result<Vec<Row>, tokio_postgres::Error> {
    let st = tx.prepare_cached(sql).await?;
    tx.query(&st, params).await
}

/// Run a (cached) prepared statement inside a transaction; returns the affected row count.
pub(crate) async fn execute(
    tx: &deadpool_postgres::Transaction<'_>,
    sql: &str,
    params: Params<'_>,
) -> Result<u64, tokio_postgres::Error> {
    let st = tx.prepare_cached(sql).await?;
    tx.execute(&st, params).await
}

/// Run a (cached) prepared query outside an explicit transaction (follower reads).
pub(crate) async fn query_implicit(
    c: &Object,
    sql: &str,
    params: Params<'_>,
) -> Result<Vec<Row>, tokio_postgres::Error> {
    let st = c.prepare_cached(sql).await?;
    c.query(&st, params).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_is_bounded() {
        for attempt in 0..40 {
            let d = backoff(attempt);
            assert!(d < BACKOFF_CAP, "attempt {attempt}: {d:?}");
        }
        assert!(backoff(0) < BACKOFF_BASE);
    }

    #[test]
    fn semantic_errors_pass_through_and_are_not_retried() {
        let e = TxErr::Scion(ScionError::not_found("x"));
        assert!(!e.retryable(true));
        assert_eq!(e.into_scion("op").code, scion_core::ErrorCode::NotFound);
        let c = TxErr::Conflict("refs");
        assert!(c.retryable(false));
        assert_eq!(c.into_scion("cas_ref").subcode(), Some(subcode::TXN_RETRY));
        assert!(!TxErr::LandedUnique.retryable(false));
        let l = TxErr::LandedUnique.into_scion("cas_ref");
        assert_eq!(l.subcode(), Some(subcode::STORE_UNAVAILABLE));
    }
}
