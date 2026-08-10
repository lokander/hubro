use std::io::Write;
use std::path::Path;

use sqlx::sqlite::SqlitePool;

use super::caps::{self, Capabilities, TableAccess, WriteProtection};
use super::ddl::{Ddl, DdlObject};
use super::error::DbError;
use super::export::ExportFormat;
use super::page::{classify_column, ColumnClass, Page, PageRequest, PREVIEW_BYTES};
use super::postgres;
use super::rowkey::{detect_row_identity, RowIdentity};
use super::schema::{ColumnMeta, TableMeta};
use super::sql::{cell_fetch_sql, equalities_where, value_len, Dialect};
use super::sqlite;
use super::sqlserver::{self, MssqlPool, MssqlTx};
use super::sqlx_common;
use super::staged::{CheckedStatement, RowLocator};
use super::value::{QueryResult, Value};

/// Hard cap on rows the free-form query path fetches into memory, independent
/// of the editor's 500-row *render* cap. A `SELECT` returning more is
/// truncated to this many rows with a "showing first N" indicator, so a query
/// against a multi-GB table can never buffer the whole result (FRE-33).
pub const MAX_QUERY_ROWS: u64 = 10_000;

/// Per-cell byte cap applied while streaming the free-form query path, so a
/// `SELECT *` returning huge individual cells can't blow memory even within
/// the row cap. Generous enough to show any reasonable value; the result is
/// read-only (never staged/exported through this), so the trim is display-only.
pub const QUERY_CELL_CAP: usize = 64 * 1024;

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

/// A backend-neutral transaction handle for atomically running a script
/// (FRE-38): every statement runs on the one held connection, so they share a
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
    // BEGIN/COMMIT as manual SQL (tiberius has no transaction API). Boxed:
    // the checked-out client is large next to the slim sqlx handles.
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

    /// Runs a row-returning statement in the transaction, bounded exactly like
    /// the pool's [`DbPool::query_capped`] (row cap + per-cell byte cap).
    pub async fn query_capped(
        &mut self,
        sql: &str,
        max_rows: u64,
    ) -> Result<(QueryResult, bool), DbError> {
        // `tx` deref-coerces to the `&mut Connection` these helpers take.
        match self {
            ScriptTx::Sqlite(tx) => {
                sqlite::query_capped_conn(tx, sql, max_rows, QUERY_CELL_CAP).await
            }
            ScriptTx::Postgres(tx) => {
                postgres::query_capped_conn(tx, sql, max_rows, QUERY_CELL_CAP).await
            }
            ScriptTx::SqlServer(tx) => {
                sqlserver::query_capped_conn(tx, sql, max_rows, QUERY_CELL_CAP).await
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

impl DbPool {
    /// Opens an existing SQLite database file.
    pub async fn open_sqlite(path: &Path) -> Result<DbPool, DbError> {
        Ok(DbPool::Sqlite(sqlite::open_sqlite(path).await?))
    }

    /// Connects to a Postgres URL (password already spliced in if any).
    pub async fn open_postgres(url: &str) -> Result<DbPool, DbError> {
        Ok(DbPool::Postgres(postgres::open_postgres(url).await?))
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

    /// Which engine answered on a Postgres-wire connection (FRE-90), or `None`
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

    /// This connection's default capabilities (FRE-87). The three drivers are
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
    /// and `ROLLBACK` does nothing (FRE-93). Declaring that here is what makes
    /// the script tab stop wrapping batches
    /// ([`wrap_atomically`](super::script::wrap_atomically)) and editing
    /// refuse rather than run unguarded
    /// ([`NO_GUARDED_WRITE`](super::caps::NO_GUARDED_WRITE)).
    ///
    /// **This is what the engine can do, not what this connection may do.**
    /// It knows nothing about the user's write protection (FRE-111), so a
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
                    ..Capabilities::FULL
                },
                // Stock Postgres and the rest of the family hold real
                // transactions — including Materialize, whose rollback is
                // genuine (FRE-92), and CockroachDB and YugabyteDB, whose
                // rollback covers DML and merely lets DDL escape (FRE-146).
                postgres::PgFlavor::Postgres
                | postgres::PgFlavor::CockroachDB
                | postgres::PgFlavor::Yugabyte
                | postgres::PgFlavor::Materialize => Capabilities::FULL,
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
    /// ends up silently ignoring the user's marking (FRE-111).
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
    /// certainly the FRE-111 bug this prefix exists to make visible.
    pub fn backend_access(&self, table: &TableMeta) -> TableAccess {
        TableAccess::resolve(self.backend_capabilities(), table, self.dialect())
    }

    /// Runs a free-form read query, buffered. A zero-row result still carries
    /// its column headers (FRE-138) — this is a user-facing result set, unlike
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

    /// Opens a transaction for atomically running a multi-statement script
    /// (FRE-38). The returned [`ScriptTx`] runs statements on one connection so
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
    /// Staged edits (FRE-14) apply through this so a batch either lands
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
    /// text / json / blobs): the grid's read path (FRE-33). `columns` are the
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
    /// capping each cell to [`QUERY_CELL_CAP`] bytes (FRE-33). Returns the
    /// bounded result plus whether rows were dropped past the cap, so the SQL
    /// editor can show a "showing first N" indicator. Pulls rows one at a time
    /// (`fetch` + `try_next`) — the same streaming primitive as [`Self::export`].
    pub async fn query_capped(
        &self,
        sql: &str,
        params: &[Value],
        max_rows: u64,
    ) -> Result<(QueryResult, bool), DbError> {
        match self {
            DbPool::Sqlite(pool) => {
                sqlite::query_capped(pool, sql, params, max_rows, QUERY_CELL_CAP).await
            }
            DbPool::Postgres(pg) => {
                postgres::query_capped(pg.pool(), sql, params, max_rows, QUERY_CELL_CAP).await
            }
            DbPool::SqlServer(pool) => {
                sqlserver::query_capped(pool, sql, params, max_rows, QUERY_CELL_CAP).await
            }
        }
    }

    /// Loads one cell's full value on demand — the lazy counterpart to the
    /// grid's bounded page fetch (FRE-33). Builds a targeted
    /// `SELECT <col> FROM <table> WHERE <full key> = …` from the row's
    /// [`RowLocator`], previewing at [`FETCH_CELL_MAX_BYTES`] so even a
    /// multi-GB cell returns a bounded prefix (flagged [`CellFetch::capped`])
    /// rather than the whole thing. Used by cell expand and by the editor when
    /// opening a truncated cell (so a preview is never staged as an edit).
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
        // Bind the row's key values as text, mirroring how the equalities page
        // filter / foreign-key jumps pin a row (exotic key types compare too).
        let pairs: Vec<(String, Value)> = identity
            .key_columns()
            .iter()
            .zip(locator.identity_values.iter())
            .map(|(col, value)| ((*col).to_string(), value.clone()))
            .collect();
        let (where_clause, params) = equalities_where(&pairs, dialect);
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
        let result = self.query_with(&sql, &params).await?;
        match result.rows.first().and_then(|r| r.first()) {
            Some(Value::Integer(n)) => Ok(*n as u64),
            other => Err(DbError::Query(format!(
                "unexpected COUNT(*) result: {other:?}"
            ))),
        }
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
    /// its indexes (FRE-108). Prefers the server's own definition and only
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
/// first failure transient ([`DbError::Transient`], FRE-147).
///
/// The two operations wrapped in this are the multi-statement reads:
/// [`DbPool::introspect`] issues six catalog queries and
/// [`DbPool::fetch_ddl`] up to three. Each runs in its own implicit
/// transaction on a pooled connection, so a schema change landing *between*
/// them can fail the call — on YugabyteDB the connection's catalog snapshot is
/// invalidated outright. Nothing the user did, nothing they can act on, and
/// the message names an engine internal.
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

/// Refuses the grid's paged reads when `caps` can't express them (FRE-87).
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
    /// The user's write protection for this connection (FRE-111), copied from
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
    /// This connection's **effective** capabilities: what the backend declares
    /// (FRE-87), narrowed by the user's write protection (FRE-111).
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

    /// Re-marks an open connection after the user edits its saved entry
    /// (FRE-111), so a tab that is already open starts obeying the new
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
    async fn attempts(outcomes: &[Result<u8, DbError>]) -> (Result<u8, DbError>, usize) {
        let calls = std::cell::Cell::new(0);
        let result = retry_transient(|| {
            let index = calls.get();
            calls.set(index + 1);
            let outcome = outcomes[index.min(outcomes.len() - 1)].clone();
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
