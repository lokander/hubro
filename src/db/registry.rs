use std::io::Write;
use std::path::Path;

use sqlx::sqlite::SqlitePool;

use super::caps::{self, Capabilities, TableAccess, WriteProtection};
use super::ddl::{Ddl, DdlObject};
use super::error::DbError;
use super::export::ExportFormat;
use super::page::{classify_column, ColumnClass, Page, PageRequest, PREVIEW_BYTES};
use super::plan;
use super::postgres;
use super::rowkey::{detect_row_identity, RowIdentity};
use super::schema::{ColumnMeta, TableMeta};
use super::sql::{cell_fetch_sql, value_len, Dialect};
use super::sqlite;
use super::sqlserver::{self, MssqlPool, MssqlTx};
use super::sqlx_common;
use super::staged::{self, CheckedStatement, RowLocator};
use super::stats::TableStats;
use super::value::{QueryResult, Value};

/// Hard cap on rows the free-form query path fetches into memory, independent
/// of the editor's 500-row *render* cap. A `SELECT` returning more is
/// truncated to this many rows with a "showing first N" indicator, so a query
/// against a multi-GB table can never buffer the whole result.
pub const MAX_QUERY_ROWS: u64 = 10_000;

/// Per-cell byte cap applied while streaming the free-form query path, so a
/// `SELECT *` returning huge individual cells can't blow memory even within
/// the row cap. Generous enough to show any reasonable value; the result is
/// read-only (never staged/exported through this), so the trim is display-only.
pub const QUERY_CELL_CAP: usize = 64 * 1024;

/// Per-cell byte cap for an `EXPLAIN` statement's output on the free-form
/// query path, in place of [`QUERY_CELL_CAP`].
///
/// A plan is a document, not a value to preview: PostgreSQL returns a whole
/// `EXPLAIN (FORMAT JSON)` plan in one cell, and a prefix of it does not
/// parse, so the plan view fell back to raw JSON cut off mid-string.
/// Measured on PostgreSQL 17, a `SELECT *` over a table with 400 partitions
/// plans to 89,966 bytes as hubro decodes it (compact JSON; 200 partitions
/// came to 44,965) — past the 64 KiB display cap. Its size follows the
/// statement being explained rather than the data, so it is bounded by what
/// the user wrote; the same 8 MiB as [`FETCH_CELL_MAX_BYTES`] still bounds it,
/// and the plan view says so when a plan reaches it.
pub const PLAN_CELL_CAP: usize = FETCH_CELL_MAX_BYTES;

/// Cap on the full value [`DbPool::fetch_cell`] loads for a cell expand/edit.
/// A value larger than this comes back as an 8 MiB prefix flagged
/// [`CellFetch::capped`]; the editor then refuses to stage it (staging a
/// prefix would silently truncate the stored value — data loss), and the
/// expand overlay notes the value is too large to show in full.
pub const FETCH_CELL_MAX_BYTES: usize = 8 * 1024 * 1024;

/// One cell's value loaded on demand ([`DbPool::fetch_cell`]).
#[derive(Debug, Clone, PartialEq)]
pub struct CellFetch {
    /// The value — the complete cell unless [`Self::capped`] is set, in which
    /// case it is only the first [`FETCH_CELL_MAX_BYTES`].
    pub value: Value,
    /// The underlying value's full length (chars for text, bytes for blob).
    pub full_len: u64,
    /// True when the value exceeded the fetch cap and was truncated — it must
    /// NOT be staged as an edit.
    pub capped: bool,
}

/// Stable handle for one open connection (one tab in the UI).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ConnectionId(u64);

/// A pool for one open database. Cheap to clone (drivers use `Arc`
/// internally), so async tasks can grab a copy instead of borrowing state
/// across an await point.
#[derive(Clone)]
pub enum DbPool {
    Sqlite(SqlitePool),
    Postgres(postgres::PgConn),
    SqlServer(MssqlPool),
}

/// A backend-neutral transaction handle for atomically running a script:
/// every statement runs on the one held connection, so they share a
/// transaction that [`Self::commit`] or [`Self::rollback`] resolves as a unit.
///
/// Each method body is pure per-variant dispatch into that backend's module
/// helpers — nothing here assumes sqlx, so a variant with a completely
/// different shape (e.g. an owned pooled connection driving `BEGIN`/`COMMIT`
/// as manual SQL) can be added without touching the existing arms.
pub enum ScriptTx<'p> {
    Sqlite(sqlx::Transaction<'p, sqlx::Sqlite>),
    Postgres(sqlx::Transaction<'p, sqlx::Postgres>),
    // Not an sqlx transaction: an owned pooled connection driving
    // BEGIN/COMMIT as manual SQL (predating tiberius 0.13's transaction API,
    // which is unused). Boxed: the checked-out client is large next to the
    // slim sqlx handles.
    SqlServer(Box<MssqlTx>),
}

impl ScriptTx<'_> {
    /// Runs a non-row statement in the transaction, returning affected rows.
    pub async fn execute(&mut self, sql: &str) -> Result<u64, DbError> {
        // `tx` deref-coerces to the `&mut Connection` these helpers take.
        match self {
            ScriptTx::Sqlite(tx) => sqlite::execute_conn(tx, sql).await,
            ScriptTx::Postgres(tx) => postgres::execute_conn(tx, sql).await,
            ScriptTx::SqlServer(tx) => sqlserver::execute_conn(tx, sql).await,
        }
    }

    /// Runs a non-row statement with bound parameters in the transaction,
    /// returning affected rows.
    ///
    /// The parameterized counterpart to [`Self::execute`], for writes whose
    /// values must not be interpolated into the SQL and whose atomicity spans
    /// several statements: a file import sends one batch of rows per
    /// call and commits (or rolls back) all of them together. Scripts
    /// are raw text and use [`Self::execute`].
    pub async fn execute_with(&mut self, sql: &str, params: &[Value]) -> Result<u64, DbError> {
        // `tx` deref-coerces to the `&mut Connection` these helpers take.
        match self {
            ScriptTx::Sqlite(tx) => sqlite::execute_with_conn(tx, sql, params).await,
            ScriptTx::Postgres(tx) => postgres::execute_with_conn(tx, sql, params).await,
            ScriptTx::SqlServer(tx) => sqlserver::execute_with_conn(tx, sql, params).await,
        }
    }

    /// Runs a row-returning statement in the transaction, bounded exactly like
    /// the pool's [`DbPool::query_capped`] (row cap + per-cell byte cap).
    pub async fn query_capped(
        &mut self,
        sql: &str,
        max_rows: u64,
    ) -> Result<(QueryResult, bool), DbError> {
        self.query_capped_to(sql, max_rows, QUERY_CELL_CAP).await
    }

    /// [`Self::query_capped`] with the per-cell cap chosen by the caller.
    pub(super) async fn query_capped_to(
        &mut self,
        sql: &str,
        max_rows: u64,
        cell_cap: usize,
    ) -> Result<(QueryResult, bool), DbError> {
        // `tx` deref-coerces to the `&mut Connection` these helpers take.
        match self {
            ScriptTx::Sqlite(tx) => sqlite::query_capped_conn(tx, sql, max_rows, cell_cap).await,
            ScriptTx::Postgres(tx) => {
                postgres::query_capped_conn(tx, sql, max_rows, cell_cap).await
            }
            ScriptTx::SqlServer(tx) => {
                sqlserver::query_capped_conn(tx, sql, max_rows, cell_cap).await
            }
        }
    }

    /// Commits the transaction — the script's statements all take effect.
    pub async fn commit(self) -> Result<(), DbError> {
        match self {
            // The two sqlx variants resolve identically, so they share one
            // generic helper rather than one wrapper each.
            ScriptTx::Sqlite(tx) => sqlx_common::commit_tx(tx).await,
            ScriptTx::Postgres(tx) => sqlx_common::commit_tx(tx).await,
            ScriptTx::SqlServer(tx) => sqlserver::commit_tx(*tx).await,
        }
    }

    /// Rolls the transaction back — none of the script's statements persist.
    /// Best-effort: a rollback failure leaves nothing committed anyway (the
    /// transaction also rolls back on drop), so the original error is what the
    /// caller reports.
    pub async fn rollback(self) {
        match self {
            ScriptTx::Sqlite(tx) => sqlx_common::rollback_tx(tx).await,
            ScriptTx::Postgres(tx) => sqlx_common::rollback_tx(tx).await,
            ScriptTx::SqlServer(tx) => sqlserver::rollback_tx(*tx).await,
        }
    }
}

/// One script's hold on a connection the *server* will not let write: what
/// [`run_script`](super::script::run_script) runs a script through when the
/// connection's effective capabilities forbid writing
/// ([`engine_enforces_read_only`]).
///
/// The capability gate ([`script_refusal`](super::script::script_refusal))
/// refuses a write by reading the statement, and so can be wrong about it:
/// a write it cannot see (`SELECT setval(…)`), or one hidden by a place where
/// hubro's lexer and the engine's disagree about where a string or comment
/// ends. This is the backstop that does not depend on reading anything — the
/// engine refuses the write itself.
///
/// - **SQLite**: a separate connection opened read-only
///   ([`sqlite::open_read_only`]).
/// - **Postgres-wire**: one pooled connection, with **each statement in a
///   read-only transaction of its own**, opened by hubro and always rolled
///   back. Per statement rather than per script, because the backstop must not
///   depend on anything a statement can change. A session default
///   (`default_transaction_read_only`) was measured to be undone by
///   `SELECT set_config('default_transaction_read_only', 'off', false)`, and
///   to be overridden by `BEGIN READ WRITE` — both of which the gate passes as
///   reads and transaction control — and a script-long transaction ends at the
///   script's own `COMMIT`. A statement inside hubro's transaction can end it
///   or change its mode (`SET TRANSACTION READ WRITE` before any query is
///   allowed), but only as the whole of that statement: the extended protocol
///   sqlx uses takes exactly one command per statement, so nothing of the
///   script's runs before hubro's rollback closes the transaction and the
///   next statement gets a new one.
///
/// SQL Server has no read-only session or transaction mode at all, so
/// [`DbPool::begin_read_only_run`] has nothing to return there and a marked
/// SQL Server connection is still protected by the gate alone.
pub(super) enum ReadOnlyRun {
    Sqlite(sqlx::SqliteConnection),
    Postgres {
        conn: sqlx::pool::PoolConnection<sqlx::Postgres>,
        /// [`postgres::read_only_begin`] for this server.
        begin: &'static str,
    },
}

impl ReadOnlyRun {
    /// Runs a row-returning statement, bounded like [`DbPool::query_capped`]
    /// with the per-cell cap chosen by the caller.
    pub(super) async fn query_capped_to(
        &mut self,
        sql: &str,
        max_rows: u64,
        cell_cap: usize,
    ) -> Result<(QueryResult, bool), DbError> {
        match self {
            ReadOnlyRun::Sqlite(conn) => {
                sqlite::query_capped_conn(conn, sql, max_rows, cell_cap).await
            }
            ReadOnlyRun::Postgres { conn, begin } => {
                let mut tx = postgres::begin_read_only(conn, begin).await?;
                let result = postgres::query_capped_conn(&mut tx, sql, max_rows, cell_cap).await;
                read_only_outcome(result, postgres::end_read_only(tx).await)
            }
        }
    }

    /// Runs a non-row statement, returning affected rows. On a connection
    /// that may not write, only a statement the gate passed reaches this —
    /// transaction control, say — and the engine refuses one that writes.
    pub(super) async fn execute(&mut self, sql: &str) -> Result<u64, DbError> {
        match self {
            ReadOnlyRun::Sqlite(conn) => sqlite::execute_conn(conn, sql).await,
            ReadOnlyRun::Postgres { conn, begin } => {
                let mut tx = postgres::begin_read_only(conn, begin).await?;
                let result = postgres::execute_conn(&mut tx, sql).await;
                read_only_outcome(result, postgres::end_read_only(tx).await)
            }
        }
    }

    /// Lets go of the connection: closes SQLite's own, returns the pooled one.
    pub(super) async fn close(self) {
        match self {
            ReadOnlyRun::Sqlite(conn) => {
                let _ = sqlx::Connection::close(conn).await;
            }
            ReadOnlyRun::Postgres { .. } => {}
        }
    }
}

/// A read-only statement's outcome, given how its rollback went: the
/// statement's own error first — it is the one that says what happened, where
/// a failed rollback after it usually only says the connection is gone (a
/// statement that ran `pg_terminate_backend(pg_backend_pid())`, say) — then
/// the rollback's, so a statement is never reported as done when its
/// transaction may still be open.
///
/// Nothing else is needed for the connection: a failed rollback leaves sqlx's
/// `Transaction` open, its drop queues another `ROLLBACK`, and the pool pings
/// a connection on release, which sends that rollback or discards a dead
/// connection. `a_marked_connection_recovers_from_a_statement_that_kills_it`
/// pins the result.
fn read_only_outcome<T>(
    result: Result<T, DbError>,
    ended: Result<(), DbError>,
) -> Result<T, DbError> {
    let value = result?;
    ended?;
    Ok(value)
}

/// Whether this dialect's engine has a read-only mode a marked connection's
/// scripts run in ([`ReadOnlyRun`]) — the one answer both the run path and the
/// connection form's promise to the user read, so the two cannot disagree.
///
/// False for SQL Server: T-SQL has no read-only session or transaction mode.
/// `ApplicationIntent=ReadOnly` is the nearest thing and is not one — it
/// routes an availability-group connection to a readable secondary, and on
/// an ordinary server it refuses nothing (measured on SQL Server 2022:
/// `sqlcmd -K ReadOnly` created a table and inserted a row).
pub fn engine_has_read_only_mode(dialect: Dialect) -> bool {
    match dialect {
        Dialect::Sqlite | Dialect::Postgres => true,
        Dialect::SqlServer => false,
    }
}

/// Whether a script on a connection with these effective capabilities runs
/// through a [`ReadOnlyRun`], where the engine refuses writes as well as the
/// gate.
///
/// Keyed on `mutate` alone: a server's read-only mode refuses row changes and
/// schema changes together, and today `mutate` is false exactly when the user
/// marked the connection read-only ([`WriteProtection::apply`] clears both).
/// A connection permitted schema changes but not row changes would lose its
/// schema changes here — no backend declares one.
pub(super) fn engine_enforces_read_only(caps: Capabilities) -> bool {
    !caps.mutate
}

impl DbPool {
    /// Opens an existing SQLite database file.
    pub async fn open_sqlite(path: &Path) -> Result<DbPool, DbError> {
        Ok(DbPool::Sqlite(sqlite::open_sqlite(path).await?))
    }

    /// Connects to a Postgres URL (password already spliced in if any).
    pub async fn open_postgres(url: &str) -> Result<DbPool, DbError> {
        Ok(DbPool::Postgres(postgres::open_postgres(url).await?))
    }

    /// Connects to a Postgres URL on a pool of exactly one connection, so every
    /// statement shares one session. The app never opens this — one connection
    /// would queue the grid behind introspection — but a test of session-local
    /// state (a temp table) needs it to be deterministic: see
    /// [`postgres::open_postgres_sized`].
    pub async fn open_postgres_single_session(url: &str) -> Result<DbPool, DbError> {
        Ok(DbPool::Postgres(
            postgres::open_postgres_sized(url, 1).await?,
        ))
    }

    /// Connects to a SQL Server URL (password already spliced in if any).
    pub async fn open_mssql(url: &str) -> Result<DbPool, DbError> {
        Ok(DbPool::SqlServer(sqlserver::open_mssql(url).await?))
    }

    /// Connects to a SQL Server URL with explicit auth (password vs Entra
    /// token) and an optional TLS host override for tunneled connects (see
    /// [`sqlserver::open_mssql_with`]).
    pub async fn open_mssql_with(
        url: &str,
        auth: &sqlserver::MssqlAuth,
        tls_host: Option<&str>,
    ) -> Result<DbPool, DbError> {
        Ok(DbPool::SqlServer(
            sqlserver::open_mssql_with(url, auth, tls_host).await?,
        ))
    }

    pub fn dialect(&self) -> Dialect {
        match self {
            DbPool::Sqlite(_) => Dialect::Sqlite,
            DbPool::Postgres(_) => Dialect::Postgres,
            DbPool::SqlServer(_) => Dialect::SqlServer,
        }
    }

    /// Which engine answered on a Postgres-wire connection, or `None`
    /// on a backend where the question doesn't arise.
    ///
    /// Deliberately not a branching point for callers: what varies by flavor
    /// is settled inside the backend, at connect time and during
    /// introspection, so that a new engine is handled in one place rather than
    /// wherever someone remembered to check. This exists to *report* the
    /// answer — the engine tests assert on it, and it is what a future
    /// connection-details panel would show.
    pub fn pg_flavor(&self) -> Option<postgres::PgFlavor> {
        match self {
            DbPool::Postgres(pg) => Some(pg.flavor()),
            DbPool::Sqlite(_) | DbPool::SqlServer(_) => None,
        }
    }

    /// How this connection produces a query plan, or `None` when it
    /// has no `EXPLAIN` statement to produce one with — see
    /// [`NO_EXPLAIN`](super::plan::NO_EXPLAIN) for why SQL Server is that
    /// case.
    ///
    /// Kept here rather than derived from [`Self::dialect`] at the call site
    /// because it is a fact about the *server*, not the driver: the
    /// Postgres-wire engines that are not PostgreSQL get the portable
    /// spelling, in the shape [`Self::backend_capabilities`] established for
    /// the same reason.
    pub fn explain_support(&self) -> Option<plan::ExplainSupport> {
        match self {
            DbPool::Sqlite(_) => Some(plan::ExplainSupport::SQLITE),
            DbPool::Postgres(pg) => Some(match pg.flavor() {
                postgres::PgFlavor::Postgres => plan::ExplainSupport::PG_JSON,
                postgres::PgFlavor::CockroachDB
                | postgres::PgFlavor::Yugabyte
                | postgres::PgFlavor::Materialize
                | postgres::PgFlavor::RisingWave => plan::ExplainSupport::PG_TEXT,
            }),
            DbPool::SqlServer(_) => None,
        }
    }

    /// This connection's default capabilities. The three drivers are
    /// full-featured OLTP engines — they query, write, run DDL, hold
    /// transactions and page by `LIMIT`/`OFFSET` — so each declares
    /// [`Capabilities::FULL`]. Objects that are nonetheless not writable
    /// (views, key-less tables) narrow this per object in
    /// [`TableAccess::resolve`].
    ///
    /// A *server* reached through one of those drivers may still be narrower,
    /// which is what the Postgres arm is for: RisingWave speaks the Postgres
    /// wire protocol and has real editable tables, but no read-write
    /// transactions at all — `BEGIN` raises a notice saying none was started
    /// and `ROLLBACK` does nothing. Declaring that here is what makes
    /// the script tab stop wrapping batches
    /// ([`wrap_atomically`](super::script::wrap_atomically)) and editing
    /// refuse rather than run unguarded
    /// ([`NO_GUARDED_WRITE`](super::caps::NO_GUARDED_WRITE)).
    ///
    /// **This is what the engine can do, not what this connection may do.**
    /// It knows nothing about the user's write protection, so a
    /// gate that consults it lets a write through on a connection marked
    /// read-only. Gates want
    /// [`Connection::capabilities`] — hence the `backend_` prefix on both of
    /// these, which is the whole reason they carry it.
    pub fn backend_capabilities(&self) -> Capabilities {
        match self {
            DbPool::Sqlite(_) | DbPool::SqlServer(_) => Capabilities::FULL,
            DbPool::Postgres(pg) => match pg.flavor() {
                postgres::PgFlavor::RisingWave => Capabilities {
                    transactions: false,
                    // Nothing rolls back here at all, so schema changes
                    // certainly don't.
                    transactional_ddl: false,
                    ..Capabilities::FULL
                },
                // Real transactions that cover DML, and schema changes that
                // escape them anyway. CockroachDB commits the open
                // transaction *before* each DDL statement
                // (`autocommit_before_ddl`, on by default), so the schema
                // change and everything written before it survive a rollback;
                // YugabyteDB commits each schema change as it executes, so
                // only the DDL itself does. Both are wrong in the same
                // direction — hubro would claim a rollback undid more than it
                // did — and one flag covers both without hubro having to say
                // which engine is which.
                postgres::PgFlavor::CockroachDB | postgres::PgFlavor::Yugabyte => Capabilities {
                    transactional_ddl: false,
                    ..Capabilities::FULL
                },
                // Stock Postgres and Materialize roll back everything they
                // are given, schema changes included (Materialize's rollback
                // is genuine; `tests/db_materialize.rs` exercises it).
                postgres::PgFlavor::Postgres | postgres::PgFlavor::Materialize => {
                    Capabilities::FULL
                }
            },
        }
    }

    /// How a single row of `table` is addressed, or `None` when it has no
    /// addressable identity.
    ///
    /// This is the *read* half of [`TableAccess`] and the only half that is
    /// safe to take from the pool: which columns must be fetched whole, and
    /// how a cell fetch pins one row, are facts about the table, not about
    /// what the user is permitted to do with it. Exposing just this lets the
    /// two legitimate callers stop reaching for a full [`TableAccess`] they
    /// would then be tempted to read `can_mutate` off — which is how a gate
    /// ends up silently ignoring the user's marking.
    pub fn backend_row_identity(&self, table: &TableMeta) -> Option<RowIdentity> {
        detect_row_identity(table, self.dialect())
    }

    /// Resolves the *engine's* capabilities for one object, ignoring the
    /// user's marking.
    ///
    /// **Not a gate.** No caller in `src/` should need this: gates want
    /// [`Connection::access`], and a read path that only needs to address a
    /// row wants [`Self::backend_row_identity`]. It stays `pub` solely so the
    /// integration tests in `tests/` can build an unmarked [`TableAccess`] to
    /// drive the write paths with; if a use appears in `src/`, it is almost
    /// certainly a gate ignoring the user's read-only marking — the bug this
    /// prefix exists to make visible.
    pub fn backend_access(&self, table: &TableMeta) -> TableAccess {
        TableAccess::resolve(self.backend_capabilities(), table, self.dialect())
    }

    /// Runs a free-form read query, buffered. A zero-row result still carries
    /// its column headers — this is a user-facing result set, unlike
    /// the projection-built reads behind [`Self::query_with`].
    pub async fn query(&self, sql: &str) -> Result<QueryResult, DbError> {
        match self {
            DbPool::Sqlite(pool) => sqlite::query(pool, sql).await,
            DbPool::Postgres(pg) => postgres::query(pg.pool(), sql).await,
            // TDS reports result-set metadata for zero rows, so this backend
            // needs no separate entry point to hold the same contract.
            DbPool::SqlServer(pool) => sqlserver::query_with(pool, sql, &[]).await,
        }
    }

    /// Executes a statement without fetching rows (INSERT/UPDATE/DDL/…),
    /// returning the driver's affected-row count.
    pub async fn execute(&self, sql: &str) -> Result<u64, DbError> {
        match self {
            DbPool::Sqlite(pool) => sqlite::execute(pool, sql).await,
            DbPool::Postgres(pg) => postgres::execute(pg.pool(), sql).await,
            DbPool::SqlServer(pool) => sqlserver::execute(pool, sql).await,
        }
    }

    /// Opens a transaction for atomically running a multi-statement script.
    /// The returned [`ScriptTx`] runs statements on one connection so
    /// they share a transaction, then commits or rolls back as a unit.
    pub async fn begin_script_tx(&self) -> Result<ScriptTx<'_>, DbError> {
        match self {
            DbPool::Sqlite(pool) => pool
                .begin()
                .await
                .map(ScriptTx::Sqlite)
                .map_err(|e| DbError::Query(e.to_string())),
            DbPool::Postgres(pg) => pg
                .pool()
                .begin()
                .await
                .map(ScriptTx::Postgres)
                .map_err(|e| DbError::Query(e.to_string())),
            DbPool::SqlServer(pool) => sqlserver::begin_tx(pool)
                .await
                .map(|tx| ScriptTx::SqlServer(Box::new(tx))),
        }
    }

    /// Takes a connection for one read-only script — see [`ReadOnlyRun`].
    /// `Ok(None)` on SQL Server, which has no read-only mode to hold one in.
    pub(super) async fn begin_read_only_run(&self) -> Result<Option<ReadOnlyRun>, DbError> {
        if !engine_has_read_only_mode(self.dialect()) {
            return Ok(None);
        }
        match self {
            DbPool::Sqlite(pool) => Ok(Some(ReadOnlyRun::Sqlite(
                sqlite::open_read_only(pool).await?,
            ))),
            DbPool::Postgres(pg) => Ok(Some(ReadOnlyRun::Postgres {
                conn: pg
                    .pool()
                    .acquire()
                    .await
                    .map_err(|e| DbError::Query(e.to_string()))?,
                begin: pg.read_only_begin(),
            })),
            // Turned away above; kept as an arm so a new variant must decide.
            DbPool::SqlServer(_) => Ok(None),
        }
    }

    /// Executes a parameterized write inside a transaction and commits only
    /// when the affected-row count is exactly `expected_rows`; otherwise
    /// rolls back and returns [`DbError::RowCountMismatch`]. Row edits go
    /// through this so a statement that would touch more rows than the one
    /// being edited can never commit. (One-statement convenience over
    /// [`Self::execute_all_checked`].)
    pub async fn execute_checked(
        &self,
        sql: &str,
        params: &[Value],
        expected_rows: u64,
    ) -> Result<u64, DbError> {
        let statement = CheckedStatement {
            sql: sql.to_string(),
            params: params.to_vec(),
            expected_rows,
        };
        self.execute_all_checked(std::slice::from_ref(&statement))
            .await
            .map(|()| expected_rows)
            .map_err(|(_, error)| error)
    }

    /// Executes every statement inside ONE transaction, committing only when
    /// each affected exactly its expected row count. Any failure rolls the
    /// whole batch back; the error names the failing statement by index
    /// (`None` when opening or committing the transaction itself failed).
    /// Staged edits apply through this so a batch either lands
    /// completely or not at all.
    pub async fn execute_all_checked(
        &self,
        statements: &[CheckedStatement],
    ) -> Result<(), (Option<usize>, DbError)> {
        match self {
            DbPool::Sqlite(pool) => sqlite::execute_all_checked(pool, statements).await,
            DbPool::Postgres(pg) => postgres::execute_all_checked(pg.pool(), statements).await,
            DbPool::SqlServer(pool) => sqlserver::execute_all_checked(pool, statements).await,
        }
    }

    async fn query_with(&self, sql: &str, params: &[Value]) -> Result<QueryResult, DbError> {
        match self {
            DbPool::Sqlite(pool) => sqlite::query_with(pool, sql, params).await,
            DbPool::Postgres(pg) => postgres::query_with(pg.pool(), sql, params).await,
            DbPool::SqlServer(pool) => sqlserver::query_with(pool, sql, params).await,
        }
    }

    /// One page of a table, honoring the request's sort and filter.
    pub async fn fetch_page(&self, request: &PageRequest) -> Result<QueryResult, DbError> {
        refuse_paged_read(self.backend_capabilities())?;
        let (sql, params) = request.select_sql(self.dialect());
        self.query_with(&sql, &params).await
    }

    /// One page of a table with **bounded previews** of large columns (long
    /// text / json / blobs): the grid's read path. `columns` are the
    /// table's visible columns (introspection order); `no_preview` names
    /// columns that must be fetched whole — the row-identity key columns and
    /// foreign-key columns, whose truncation would misaddress rows or
    /// misdirect a foreign-key jump. The returned [`Page`] carries per-cell
    /// truncation metadata; the full value of a truncated cell is loaded on
    /// demand via [`Self::fetch_cell`].
    pub async fn fetch_page_bounded(
        &self,
        request: &PageRequest,
        columns: &[ColumnMeta],
        no_preview: &[&str],
    ) -> Result<Page, DbError> {
        refuse_paged_read(self.backend_capabilities())?;
        let (sql, params, plan) =
            request.select_bounded_sql(self.dialect(), columns, no_preview, PREVIEW_BYTES);
        let raw = self.query_with(&sql, &params).await?;
        Ok(plan.assemble(raw, PREVIEW_BYTES))
    }

    /// Streams an arbitrary read query, retaining at most `max_rows` rows and
    /// capping each cell to [`QUERY_CELL_CAP`] bytes. Returns the
    /// bounded result plus whether rows were dropped past the cap, so the SQL
    /// editor can show a "showing first N" indicator. Pulls rows one at a time
    /// (`fetch` + `try_next`) — the same streaming primitive as [`Self::export`].
    pub async fn query_capped(
        &self,
        sql: &str,
        params: &[Value],
        max_rows: u64,
    ) -> Result<(QueryResult, bool), DbError> {
        self.query_capped_to(sql, params, max_rows, QUERY_CELL_CAP)
            .await
    }

    /// [`Self::query_capped`] with the per-cell cap chosen by the caller.
    pub(super) async fn query_capped_to(
        &self,
        sql: &str,
        params: &[Value],
        max_rows: u64,
        cell_cap: usize,
    ) -> Result<(QueryResult, bool), DbError> {
        match self {
            DbPool::Sqlite(pool) => {
                sqlite::query_capped(pool, sql, params, max_rows, cell_cap).await
            }
            DbPool::Postgres(pg) => {
                postgres::query_capped(pg.pool(), sql, params, max_rows, cell_cap).await
            }
            DbPool::SqlServer(pool) => {
                sqlserver::query_capped(pool, sql, params, max_rows, cell_cap).await
            }
        }
    }

    /// Loads one cell's full value on demand — the lazy counterpart to the
    /// grid's bounded page fetch. Builds a targeted
    /// `SELECT <col> FROM <table> WHERE <full key> = …` from the row's
    /// [`RowLocator`], previewing at [`FETCH_CELL_MAX_BYTES`] so even a
    /// multi-GB cell returns a bounded prefix (flagged [`CellFetch::capped`])
    /// rather than the whole thing. Used by cell expand and by the editor when
    /// opening a truncated cell (so a preview is never staged as an edit).
    ///
    /// A locator whose arity no longer matches `identity` is a
    /// [`DbError::Query`], not a lookup over the columns that happen to pair
    /// up — see [`staged::locator_mismatch`].
    pub async fn fetch_cell(
        &self,
        table: &TableMeta,
        identity: &RowIdentity,
        locator: &RowLocator,
        column: &str,
    ) -> Result<CellFetch, DbError> {
        let dialect = self.dialect();
        let class = table
            .columns
            .iter()
            .find(|c| c.name == column)
            .map(|c| classify_column(&c.type_name))
            .unwrap_or(ColumnClass::Text);
        // The same predicate Save writes the row through — see
        // [`staged::row_where`] for why it is not a text comparison. A locator
        // that no longer fits the key is an error, never a narrower WHERE.
        let (where_clause, params) =
            staged::row_where(table, identity, dialect, locator).map_err(DbError::Query)?;
        let sql = cell_fetch_sql(
            dialect,
            table,
            class,
            column,
            &where_clause,
            FETCH_CELL_MAX_BYTES,
        );
        let result = self.query_with(&sql, &params).await?;
        let Some(row) = result.rows.into_iter().next() else {
            // The row is gone (concurrent delete): report an empty value
            // rather than an error.
            return Ok(CellFetch {
                value: Value::Null,
                full_len: 0,
                capped: false,
            });
        };
        let value = row.first().cloned().unwrap_or(Value::Null);
        let full_len = match row.get(1) {
            Some(Value::Integer(n)) => *n as u64,
            _ => value_len(&value),
        };
        Ok(CellFetch {
            capped: full_len > FETCH_CELL_MAX_BYTES as u64,
            value,
            full_len,
        })
    }

    /// Total row count for the request's table and filter (paging ignored).
    pub async fn count_rows(&self, request: &PageRequest) -> Result<u64, DbError> {
        let (sql, params) = request.count_sql(self.dialect());
        self.count_query(&sql, &params).await
    }

    /// Exactly how many rows `table` holds, by running `COUNT(*)`.
    ///
    /// **Only ever because the user asked.** This is the expensive half of the
    /// pair — a full scan on most engines — and the reason the schema pane's
    /// figure comes from [`Self::fetch_table_stats`] instead until an explicit
    /// action calls this. Nothing that merely opens or expands anything may
    /// reach it.
    ///
    /// Unfiltered by construction: the grid's own count runs through
    /// [`Self::count_rows`] with the page's filter, and the two must not be
    /// confused for each other.
    pub async fn count_table_rows(&self, table: &TableMeta) -> Result<u64, DbError> {
        let sql = format!(
            "SELECT COUNT(*) FROM {}",
            super::sql::qualified(table.schema.as_deref(), &table.name)
        );
        self.count_query(&sql, &[]).await
    }

    /// Runs a single-value `COUNT(*)` query and decodes it.
    async fn count_query(&self, sql: &str, params: &[Value]) -> Result<u64, DbError> {
        let result = self.query_with(sql, params).await?;
        match result.rows.first().and_then(|r| r.first()) {
            Some(Value::Integer(n)) => Ok(*n as u64),
            other => Err(DbError::Query(format!(
                "unexpected COUNT(*) result: {other:?}"
            ))),
        }
    }

    /// Cheap storage statistics for one object: an estimated row count and its
    /// on-disk size, read from statistics the server already keeps.
    ///
    /// **Never scans.** Each backend answers from one catalog query, so this is
    /// safe to run whenever a table is merely looked at — which is the whole
    /// point, since the exact count is not. Anything the server does not keep
    /// comes back as `None` on [`TableStats`] rather than as a zero.
    ///
    /// Retried once on a transient failure like the other multi-statement
    /// catalog reads: the Postgres arm probes the catalog's shape before
    /// querying it, so a schema change can land between the two.
    pub async fn fetch_table_stats(&self, table: &TableMeta) -> Result<TableStats, DbError> {
        retry_transient(|| async {
            match self {
                DbPool::Sqlite(pool) => sqlite::fetch_table_stats(pool, table).await,
                DbPool::Postgres(pg) => postgres::fetch_table_stats(pg.pool(), table).await,
                DbPool::SqlServer(pool) => sqlserver::fetch_table_stats(pool, table).await,
            }
        })
        .await
    }

    /// Streams the rows of `sql` (with bound `params`) to `out` in `format`,
    /// pulling one row at a time so the export never buffers the full result.
    /// Returns the number of data rows written. See [`export`](super::export).
    pub async fn export(
        &self,
        sql: &str,
        params: &[Value],
        format: ExportFormat,
        out: &mut impl Write,
    ) -> Result<u64, DbError> {
        match self {
            DbPool::Sqlite(pool) => sqlite::export(pool, sql, params, format, out).await,
            DbPool::Postgres(pg) => postgres::export(pg.pool(), sql, params, format, out).await,
            DbPool::SqlServer(pool) => sqlserver::export(pool, sql, params, format, out).await,
        }
    }

    /// DDL for one of `table`'s objects — the table/view itself, or one of
    /// its indexes. Prefers the server's own definition and only
    /// reconstructs where the backend has no generator; the returned [`Ddl`]
    /// says which, and [`Ddl::text`] labels a reconstruction.
    pub async fn fetch_ddl(&self, table: &TableMeta, object: &DdlObject) -> Result<Ddl, DbError> {
        retry_transient(|| async {
            match self {
                DbPool::Sqlite(pool) => sqlite::fetch_ddl(pool, table, object).await,
                DbPool::Postgres(pg) => postgres::fetch_ddl(pg.pool(), table, object).await,
                DbPool::SqlServer(pool) => sqlserver::fetch_ddl(pool, table, object).await,
            }
        })
        .await
    }

    pub async fn introspect(&self) -> Result<Vec<TableMeta>, DbError> {
        retry_transient(|| async {
            match self {
                DbPool::Sqlite(pool) => sqlite::introspect(pool).await,
                DbPool::Postgres(pg) => postgres::introspect(pg).await,
                DbPool::SqlServer(pool) => sqlserver::introspect(pool).await,
            }
        })
        .await
    }

    pub async fn close(&self) {
        match self {
            DbPool::Sqlite(pool) => pool.close().await,
            DbPool::Postgres(pg) => pg.pool().close().await,
            DbPool::SqlServer(pool) => pool.close().await,
        }
    }
}

/// Runs a catalog read, running it a second time if the server called the
/// first failure transient ([`DbError::Transient`]).
///
/// The three operations wrapped in this are the multi-statement reads.
/// [`DbPool::introspect`] is the one this exists for: its catalog queries each
/// run in their own implicit transaction on a pooled connection, so a schema
/// change landing *between* them can fail the whole call — on YugabyteDB the
/// connection's catalog snapshot is invalidated outright. Nothing the user did,
/// nothing they can act on, and the message names an engine internal.
///
/// [`DbPool::fetch_ddl`] is wrapped at the same seam for the branches that can
/// return an error at all — the single-query index and view lookups. Its
/// table branch cannot: a failed catalog read there is folded into the output
/// as a caveat rather than propagated, so it degrades instead of retrying, and
/// deliberately so. [`DbPool::fetch_table_stats`] joins them for the same
/// reason as introspection: its Postgres arm probes which statistics the
/// catalog keeps and then reads them, two statements with a gap between.
///
/// **Once, never in a loop.** A second failure means something other than a
/// racing schema change, and a schema tree that hangs is worse than one that
/// reports an error. The retry is also confined to *reads*: `40001` on a write
/// is a conflict the user must be told about, not one to silently paper over
/// by running the write again.
///
/// Backend-agnostic on purpose. Only the Postgres backend builds
/// [`DbError::Transient`] today, so this is a plain call-through for SQLite and
/// SQL Server — but the policy is one function, so a fourth backend that has a
/// retryable class inherits it by classifying its errors, not by repeating this.
async fn retry_transient<T, F, Fut>(mut operation: F) -> Result<T, DbError>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T, DbError>>,
{
    match operation().await {
        Err(err) if err.is_transient() => operation().await,
        result => result,
    }
}

/// Refuses the grid's paged reads when `caps` can't express them.
///
/// A connection that can't query at all has nothing to page; and both paged
/// selects append a `LIMIT`/`OFFSET` tail unconditionally, so a backend that
/// pages by cursor instead must not reach them. The unpaged reads
/// (`count_rows`, `export`) are unaffected by the paging half.
fn refuse_paged_read(caps: Capabilities) -> Result<(), DbError> {
    if !caps.read_query {
        return Err(DbError::Unsupported(caps::NO_QUERY.to_string()));
    }
    if !caps.offset_paging {
        return Err(DbError::Unsupported(caps::NO_OFFSET_PAGING.to_string()));
    }
    Ok(())
}

/// One open connection: a display name plus its pool.
#[derive(Clone)]
pub struct Connection {
    pub id: ConnectionId,
    pub name: String,
    pub pool: DbPool,
    /// The user's write protection for this connection, copied from
    /// the [`SavedConnection`](crate::config::SavedConnection) it was opened
    /// from. It lives here rather than beside the pool because every gated
    /// path already has the connection in hand — making it hard to resolve
    /// capabilities while forgetting the protection.
    ///
    /// The connection's *accent colour* deliberately does not live here: it
    /// warns, it doesn't enforce, so it stays a presentation concern in
    /// `ui::state` and never reaches the `db/` layer.
    pub protection: WriteProtection,
}

impl Connection {
    /// This connection's **effective** capabilities: what the backend declares,
    /// narrowed by the user's write protection.
    ///
    /// Every capability gate should read this rather than
    /// [`DbPool::capabilities`], which reports only what the engine can do.
    pub fn capabilities(&self) -> Capabilities {
        self.protection.apply(self.pool.backend_capabilities())
    }

    /// Resolves this connection's effective capabilities for one object — the
    /// single entry point the UI and the write paths gate on.
    pub fn access(&self, table: &TableMeta) -> TableAccess {
        TableAccess::resolve_protected(
            self.pool.backend_capabilities(),
            self.protection,
            table,
            self.pool.dialect(),
        )
    }

    /// Whether a write through this connection must be confirmed first.
    pub fn confirms_writes(&self) -> bool {
        self.protection.confirms()
    }
}

/// All simultaneously open connections, in tab order.
///
/// Sync by design: open pools first (await), then insert. The registry lives
/// in a signal, and inserting through `.write()` must not span an await.
#[derive(Default)]
pub struct ConnectionRegistry {
    next_id: u64,
    connections: Vec<Connection>,
}

impl ConnectionRegistry {
    pub fn insert(
        &mut self,
        name: impl Into<String>,
        pool: DbPool,
        protection: WriteProtection,
    ) -> ConnectionId {
        let id = ConnectionId(self.next_id);
        self.next_id += 1;
        self.connections.push(Connection {
            id,
            name: name.into(),
            pool,
            protection,
        });
        id
    }

    /// Re-marks an open connection after the user edits its saved entry,
    /// so a tab that is already open starts obeying the new
    /// protection without being reconnected.
    pub fn set_protection(&mut self, id: ConnectionId, protection: WriteProtection) {
        if let Some(connection) = self.connections.iter_mut().find(|c| c.id == id) {
            connection.protection = protection;
        }
    }

    pub fn get(&self, id: ConnectionId) -> Option<&Connection> {
        self.connections.iter().find(|c| c.id == id)
    }

    /// Removes and returns the connection; callers should `close()` its pool
    /// from an async task.
    pub fn remove(&mut self, id: ConnectionId) -> Option<Connection> {
        let idx = self.connections.iter().position(|c| c.id == id)?;
        Some(self.connections.remove(idx))
    }

    pub fn iter(&self) -> impl Iterator<Item = &Connection> {
        self.connections.iter()
    }

    pub fn len(&self) -> usize {
        self.connections.len()
    }

    pub fn is_empty(&self) -> bool {
        self.connections.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_read_only_statement_reports_its_own_error_before_its_rollbacks() {
        let statement = || DbError::Query("terminating connection".into());
        let rollback = || DbError::Query("connection closed".into());
        assert_eq!(
            read_only_outcome(Err::<(), _>(statement()), Err(rollback())),
            Err(statement())
        );
        assert_eq!(read_only_outcome(Ok(7), Err(rollback())), Err(rollback()));
        assert_eq!(
            read_only_outcome(Err::<(), _>(statement()), Ok(())),
            Err(statement())
        );
        assert_eq!(read_only_outcome(Ok(7), Ok(())), Ok(7));
    }

    #[test]
    fn paged_reads_are_refused_without_querying_or_offset_paging() {
        // The three current backends are fully capable, so this gate is only
        // reachable through a synthetic set — which is exactly why it is
        // tested here rather than through a pool.
        assert_eq!(refuse_paged_read(Capabilities::FULL), Ok(()));
        // Writing has nothing to do with reading a page.
        assert_eq!(refuse_paged_read(Capabilities::FULL.read_only()), Ok(()));

        let no_paging = Capabilities {
            offset_paging: false,
            ..Capabilities::FULL
        };
        assert_eq!(
            refuse_paged_read(no_paging),
            Err(DbError::Unsupported(caps::NO_OFFSET_PAGING.to_string()))
        );

        let no_query = Capabilities {
            read_query: false,
            ..Capabilities::FULL
        };
        assert_eq!(
            refuse_paged_read(no_query),
            Err(DbError::Unsupported(caps::NO_QUERY.to_string()))
        );
    }

    /// Counts calls so each case can assert how many attempts a policy made,
    /// which is the whole content of "retry once, never in a loop".
    ///
    /// The last outcome repeats once the list runs out, so a case can say
    /// "always fails transiently" with a one-element slice — but only up to
    /// [`RUNAWAY`] calls, after which it hands back a *permanent* error. That
    /// ceiling is what a looping regression would hit: without it, feeding an
    /// endless transient failure to a policy that retried forever would hang
    /// the test run instead of failing it, and a hang says far less than a
    /// count does.
    const RUNAWAY: usize = 4;

    async fn attempts(outcomes: &[Result<u8, DbError>]) -> (Result<u8, DbError>, usize) {
        let calls = std::cell::Cell::new(0);
        let result = retry_transient(|| {
            let index = calls.get();
            calls.set(index + 1);
            let outcome = match index < RUNAWAY {
                true => outcomes[index.min(outcomes.len() - 1)].clone(),
                false => Err(DbError::Introspect("retried without bound".into())),
            };
            async move { outcome }
        })
        .await;
        (result, calls.get())
    }

    #[tokio::test]
    async fn a_transient_failure_is_retried_exactly_once() {
        let transient = || Err(DbError::Transient("catalog moved".into()));

        // Succeeding first time asks the server once.
        assert_eq!(attempts(&[Ok(1)]).await, (Ok(1), 1));

        // The case this exists for: the second attempt sees the new catalog.
        assert_eq!(attempts(&[transient(), Ok(2)]).await, (Ok(2), 2));

        // Still failing transiently is reported, not looped on. A schema tree
        // that hangs would be worse than one that says what went wrong.
        assert_eq!(
            attempts(&[transient()]).await,
            (Err(DbError::Transient("catalog moved".into())), 2)
        );

        // Everything else is a real failure and is never run twice — a
        // syntactically broken catalog query would fail identically the second
        // time, at the cost of another round trip.
        let permanent = Err(DbError::Introspect("no such column".into()));
        assert_eq!(
            attempts(std::slice::from_ref(&permanent)).await,
            (permanent, 1)
        );
    }

    fn dummy_pool() -> DbPool {
        // A lazily-connecting pool is fine for registry bookkeeping tests,
        // but sqlx still wants a Tokio context to construct it.
        DbPool::Sqlite(SqlitePool::connect_lazy("sqlite::memory:").unwrap())
    }

    #[tokio::test]
    async fn insert_assigns_unique_ids_in_tab_order() {
        let mut registry = ConnectionRegistry::default();
        let a = registry.insert("a.db", dummy_pool(), WriteProtection::Open);
        let b = registry.insert("b.db", dummy_pool(), WriteProtection::Open);
        assert_ne!(a, b);
        let names: Vec<&str> = registry.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["a.db", "b.db"]);
    }

    #[tokio::test]
    async fn remove_frees_the_entry_but_never_reuses_ids() {
        let mut registry = ConnectionRegistry::default();
        let a = registry.insert("a.db", dummy_pool(), WriteProtection::Open);
        assert!(registry.remove(a).is_some());
        assert!(registry.remove(a).is_none());
        assert!(registry.is_empty());
        let b = registry.insert("b.db", dummy_pool(), WriteProtection::Open);
        assert_ne!(a, b);
        assert_eq!(registry.len(), 1);
        assert_eq!(registry.get(b).unwrap().name, "b.db");
    }
}
