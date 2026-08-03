use std::io::Write;
use std::path::Path;

use sqlx::postgres::PgPool;
use sqlx::sqlite::SqlitePool;

use super::error::DbError;
use super::export::ExportFormat;
use super::page::{
    classify_column, equalities_where, quote_ident, ColumnClass, Dialect, Page, PageRequest,
    PREVIEW_BYTES,
};
use super::postgres;
use super::rowkey::RowIdentity;
use super::schema::{ColumnMeta, TableMeta};
use super::sqlite;
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
    Postgres(PgPool),
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
}

impl ScriptTx<'_> {
    /// Runs a non-row statement in the transaction, returning affected rows.
    pub async fn execute(&mut self, sql: &str) -> Result<u64, DbError> {
        // `tx` deref-coerces to the `&mut Connection` these helpers take.
        match self {
            ScriptTx::Sqlite(tx) => sqlite::execute_conn(tx, sql).await,
            ScriptTx::Postgres(tx) => postgres::execute_conn(tx, sql).await,
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
        }
    }

    /// Commits the transaction — the script's statements all take effect.
    pub async fn commit(self) -> Result<(), DbError> {
        match self {
            ScriptTx::Sqlite(tx) => sqlite::commit_tx(tx).await,
            ScriptTx::Postgres(tx) => postgres::commit_tx(tx).await,
        }
    }

    /// Rolls the transaction back — none of the script's statements persist.
    /// Best-effort: a rollback failure leaves nothing committed anyway (the
    /// transaction also rolls back on drop), so the original error is what the
    /// caller reports.
    pub async fn rollback(self) {
        match self {
            ScriptTx::Sqlite(tx) => sqlite::rollback_tx(tx).await,
            ScriptTx::Postgres(tx) => postgres::rollback_tx(tx).await,
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

    pub fn dialect(&self) -> Dialect {
        match self {
            DbPool::Sqlite(_) => Dialect::Sqlite,
            DbPool::Postgres(_) => Dialect::Postgres,
        }
    }

    pub async fn query(&self, sql: &str) -> Result<QueryResult, DbError> {
        match self {
            DbPool::Sqlite(pool) => sqlite::query(pool, sql).await,
            DbPool::Postgres(pool) => postgres::query_with(pool, sql, &[]).await,
        }
    }

    /// Executes a statement without fetching rows (INSERT/UPDATE/DDL/…),
    /// returning the driver's affected-row count.
    pub async fn execute(&self, sql: &str) -> Result<u64, DbError> {
        match self {
            DbPool::Sqlite(pool) => sqlite::execute(pool, sql).await,
            DbPool::Postgres(pool) => postgres::execute(pool, sql).await,
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
            DbPool::Postgres(pool) => pool
                .begin()
                .await
                .map(ScriptTx::Postgres)
                .map_err(|e| DbError::Query(e.to_string())),
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
            DbPool::Postgres(pool) => postgres::execute_all_checked(pool, statements).await,
        }
    }

    async fn query_with(&self, sql: &str, params: &[Value]) -> Result<QueryResult, DbError> {
        match self {
            DbPool::Sqlite(pool) => sqlite::query_with(pool, sql, params).await,
            DbPool::Postgres(pool) => postgres::query_with(pool, sql, params).await,
        }
    }

    /// One page of a table, honoring the request's sort and filter.
    pub async fn fetch_page(&self, request: &PageRequest) -> Result<QueryResult, DbError> {
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
            DbPool::Postgres(pool) => {
                postgres::query_capped(pool, sql, params, max_rows, QUERY_CELL_CAP).await
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
        let cap = FETCH_CELL_MAX_BYTES as i64;
        let q = quote_ident(column);
        let (value_expr, length_expr): (String, String) = match (dialect, class) {
            (_, ColumnClass::Scalar) => (q.clone(), "NULL".to_string()),
            (Dialect::Sqlite, _) => (format!("substr({q}, 1, {cap})"), format!("length({q})")),
            (Dialect::Postgres, ColumnClass::Text) => (
                format!("left({q}::text, {cap})"),
                format!("length({q}::text)"),
            ),
            (Dialect::Postgres, ColumnClass::Binary) => (
                format!("substring({q} from 1 for {cap})"),
                format!("octet_length({q})"),
            ),
        };
        // Bind the row's key values as text, mirroring how the equalities page
        // filter / foreign-key jumps pin a row (exotic key types compare too).
        let pairs: Vec<(String, Value)> = identity
            .key_columns()
            .iter()
            .zip(locator.identity_values.iter())
            .map(|(col, value)| ((*col).to_string(), value.clone()))
            .collect();
        let (where_clause, params) = equalities_where(&pairs, dialect);
        let sql = format!(
            "SELECT {value_expr}, {length_expr} FROM {}{where_clause} LIMIT 1",
            qualified_table(table),
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
            DbPool::Postgres(pool) => postgres::export(pool, sql, params, format, out).await,
        }
    }

    pub async fn introspect(&self) -> Result<Vec<TableMeta>, DbError> {
        match self {
            DbPool::Sqlite(pool) => sqlite::introspect(pool).await,
            DbPool::Postgres(pool) => postgres::introspect(pool).await,
        }
    }

    pub async fn close(&self) {
        match self {
            DbPool::Sqlite(pool) => pool.close().await,
            DbPool::Postgres(pool) => pool.close().await,
        }
    }
}

/// Schema-qualified, quoted table name for a targeted single-row read.
fn qualified_table(table: &TableMeta) -> String {
    match &table.schema {
        Some(schema) => format!("{}.{}", quote_ident(schema), quote_ident(&table.name)),
        None => quote_ident(&table.name),
    }
}

/// Fallback full-length for a value when the `length` column came back NULL
/// or non-integer (chars for text, bytes for blob, else 0).
fn value_len(value: &Value) -> u64 {
    match value {
        Value::Text(t) => t.chars().count() as u64,
        Value::Blob(b) => b.len() as u64,
        _ => 0,
    }
}

/// One open connection: a display name plus its pool.
#[derive(Clone)]
pub struct Connection {
    pub id: ConnectionId,
    pub name: String,
    pub pool: DbPool,
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
    pub fn insert(&mut self, name: impl Into<String>, pool: DbPool) -> ConnectionId {
        let id = ConnectionId(self.next_id);
        self.next_id += 1;
        self.connections.push(Connection {
            id,
            name: name.into(),
            pool,
        });
        id
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

    fn dummy_pool() -> DbPool {
        // A lazily-connecting pool is fine for registry bookkeeping tests,
        // but sqlx still wants a Tokio context to construct it.
        DbPool::Sqlite(SqlitePool::connect_lazy("sqlite::memory:").unwrap())
    }

    #[tokio::test]
    async fn insert_assigns_unique_ids_in_tab_order() {
        let mut registry = ConnectionRegistry::default();
        let a = registry.insert("a.db", dummy_pool());
        let b = registry.insert("b.db", dummy_pool());
        assert_ne!(a, b);
        let names: Vec<&str> = registry.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["a.db", "b.db"]);
    }

    #[tokio::test]
    async fn remove_frees_the_entry_but_never_reuses_ids() {
        let mut registry = ConnectionRegistry::default();
        let a = registry.insert("a.db", dummy_pool());
        assert!(registry.remove(a).is_some());
        assert!(registry.remove(a).is_none());
        assert!(registry.is_empty());
        let b = registry.insert("b.db", dummy_pool());
        assert_ne!(a, b);
        assert_eq!(registry.len(), 1);
        assert_eq!(registry.get(b).unwrap().name, "b.db");
    }
}
