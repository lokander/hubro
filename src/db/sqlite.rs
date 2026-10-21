//! SQLite backend: connecting, introspection, and query execution.
//!
//! Introspection runs a constant number of queries regardless of table count:
//! the object list comes from `sqlite_master`, and columns, indexes, and
//! foreign keys come from the pragma table-valued functions
//! (`pragma_table_xinfo`, `pragma_index_list`, `pragma_index_info`,
//! `pragma_foreign_key_list`) joined against it — no per-table `PRAGMA`
//! round-trips for ordinary tables. Views and virtual tables are the exception:
//! their columns are read one query each, so one SQLite cannot resolve costs
//! only itself (see `table_columns`).

use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::path::Path;

use sqlx::sqlite::{SqliteConnectOptions, SqlitePool, SqlitePoolOptions, SqliteRow};
use sqlx::{Row as _, TypeInfo as _, ValueRef as _};

use super::caps::Restriction;
use super::ddl::{create_index_sql, terminate, Ddl, DdlObject, IndexExtras};
use super::error::DbError;
use super::export::ExportFormat;
use super::schema::{
    ColumnMeta, ForeignKeyMeta, Generated, IndexMeta, TableKind, TableMeta, TypeDetail,
};
use super::sql::Dialect;
use super::sqlx_common::{self, get};
use super::staged::CheckedStatement;
use super::stats::{RowCount, TableStats};
use super::value::{QueryResult, Value};

/// The 16 bytes every SQLite database file begins with, including the
/// terminating NUL (the SQLite file format, section 1.3).
const SQLITE_HEADER: &[u8; 16] = b"SQLite format 3\0";

/// Why a path can't be opened as a SQLite database.
///
/// Exists so "you pointed hubro at a JPEG" is answered by hubro rather than
/// relayed from the driver: sqlx collapses a missing file, a directory and a
/// text file into `(code: 14) unable to open database file` or
/// `file is not a database`, which says neither which of those happened nor
/// which file it was.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SqliteFileError {
    /// The path as the caller gave it, so the message names the file the user
    /// typed rather than a canonicalized form they don't recognize.
    pub path: std::path::PathBuf,
    pub kind: SqliteFileErrorKind,
}

/// What is wrong with the file — see [`SqliteFileError`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SqliteFileErrorKind {
    /// Nothing exists at that path.
    NotFound,
    /// Something exists but isn't a regular file — most often a directory.
    NotAFile,
    /// A file that does not start with the SQLite header.
    NotADatabase,
    /// Existence couldn't be established (permissions, an I/O error).
    Unreadable(String),
}

impl SqliteFileError {
    /// The "nothing there" error for a path already known to be absent.
    pub fn not_found(path: std::path::PathBuf) -> Self {
        SqliteFileError {
            path,
            kind: SqliteFileErrorKind::NotFound,
        }
    }
}

impl std::fmt::Display for SqliteFileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let path = self.path.display();
        match &self.kind {
            SqliteFileErrorKind::NotFound => write!(f, "no such file: {path}"),
            SqliteFileErrorKind::NotAFile => write!(f, "not a file: {path}"),
            SqliteFileErrorKind::NotADatabase => {
                write!(f, "not a SQLite database: {path}")
            }
            SqliteFileErrorKind::Unreadable(reason) => {
                write!(f, "cannot read {path}: {reason}")
            }
        }
    }
}

impl std::error::Error for SqliteFileError {}

/// Checks that `path` is a file hubro can open as a SQLite database, without
/// opening a connection to it.
///
/// Decides on facts about the file — does it exist, is it a regular file, does
/// it start with [`SQLITE_HEADER`] — rather than on the driver's error text,
/// which is one message for four different problems. That is also what lets
/// one answer serve two places: the command line reports it before a window
/// opens, and [`open_sqlite`] turns it into the connect error the
/// connections screen shows for the file picker, the saved list, and a session
/// restore.
///
/// A **zero-length file passes**, because SQLite itself treats one as a valid
/// empty database and writes the header on first use — refusing it here would
/// reject a file the app opens perfectly well. Anything non-empty but shorter
/// than the header cannot be a database.
pub fn check_sqlite_file(path: &Path) -> Result<(), SqliteFileError> {
    let fail = |kind| {
        Err(SqliteFileError {
            path: path.to_path_buf(),
            kind,
        })
    };
    let metadata = match std::fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return fail(SqliteFileErrorKind::NotFound)
        }
        Err(err) => return fail(SqliteFileErrorKind::Unreadable(err.to_string())),
    };
    if !metadata.is_file() {
        return fail(SqliteFileErrorKind::NotAFile);
    }
    if metadata.len() == 0 {
        return Ok(());
    }
    let mut header = [0u8; SQLITE_HEADER.len()];
    // `read_exact` rather than `read`, so a short *file* and a short *read*
    // are not the same answer: a single `read` may return fewer bytes than
    // asked for on a perfectly good file, and calling that "not a database"
    // would be a lie about a readable one. Only `UnexpectedEof` — the file
    // really is shorter than a header — means what this check is looking for.
    if let Err(err) = std::fs::File::open(path).and_then(|mut file| {
        use std::io::Read as _;
        file.read_exact(&mut header)
    }) {
        return match err.kind() {
            std::io::ErrorKind::UnexpectedEof => fail(SqliteFileErrorKind::NotADatabase),
            _ => fail(SqliteFileErrorKind::Unreadable(err.to_string())),
        };
    }
    if &header != SQLITE_HEADER {
        return fail(SqliteFileErrorKind::NotADatabase);
    }
    Ok(())
}

/// Opens an existing SQLite database file and validates it is actually a
/// SQLite database.
///
/// [`check_sqlite_file`] runs first so the ordinary failures carry hubro's own
/// wording. The `PRAGMA` below still runs: a file with the right header can be
/// corrupt or encrypted past it, and that is the driver's judgement to make.
pub async fn open_sqlite(path: &Path) -> Result<SqlitePool, DbError> {
    check_sqlite_file(path).map_err(|err| DbError::Connect(err.to_string()))?;
    let options = SqliteConnectOptions::new()
        .filename(path)
        .create_if_missing(false);
    let pool = SqlitePoolOptions::new()
        .connect_with(options)
        .await
        .map_err(|e| DbError::Connect(e.to_string()))?;
    // Forces a read so "not a database" / corrupt files fail here, not later.
    sqlx::query("PRAGMA schema_version")
        .fetch_one(&pool)
        .await
        .map_err(|e| DbError::Connect(e.to_string()))?;
    Ok(pool)
}

/// Runs an arbitrary query, decoding every cell into the backend-neutral
/// [`Value`] model. Free-form SQL whose result is shown to the user, so a
/// zero-row result recovers its headers.
pub async fn query(pool: &SqlitePool, sql: &str) -> Result<QueryResult, DbError> {
    let mut result = query_with(pool, sql, &[]).await?;
    sqlx_common::fill_headers(&mut result, pool, sql).await;
    Ok(result)
}

/// Maps a driver error onto [`DbError::Query`]. sqlx 0.8 does not expose
/// `sqlite3_error_offset` (its `SqliteError` carries only code + message), so
/// failures have no position info beyond the `near "…"` context SQLite puts in
/// the message itself — unlike Postgres, which has a real error cursor to
/// enrich with.
fn query_error(err: sqlx::Error) -> DbError {
    DbError::Query(err.to_string())
}

/// Executes a statement without decoding rows, returning the driver's
/// affected-row count.
pub async fn execute(pool: &SqlitePool, sql: &str) -> Result<u64, DbError> {
    sqlx_common::execute(pool, sql, query_error).await
}

/// Executes parameterized writes inside ONE transaction, committing only
/// when every statement affected exactly its `expected_rows` rows. Any SQL
/// error or count mismatch rolls the whole batch back; the error carries the
/// index of the failing statement (`None` for begin/commit failures, which
/// belong to no statement). This is the safety net for row edits: a WHERE
/// clause that unexpectedly matches more (or fewer) rows than the one being
/// edited must never commit — and with staged edits, neither may
/// any sibling change in the same batch.
pub async fn execute_all_checked(
    pool: &SqlitePool,
    statements: &[CheckedStatement],
) -> Result<(), (Option<usize>, DbError)> {
    let mut tx = pool
        .begin()
        .await
        .map_err(|e| (None, DbError::Query(e.to_string())))?;
    for (index, statement) in statements.iter().enumerate() {
        let result = bind_params(sqlx::query(&statement.sql), &statement.params)
            .execute(&mut *tx)
            .await;
        // Dropping the transaction would roll back too; do it explicitly and
        // ignore secondary errors — the original failure is what the caller
        // needs.
        let done = match result {
            Ok(done) => done,
            Err(e) => {
                let _ = tx.rollback().await;
                return Err((Some(index), query_error(e)));
            }
        };
        let affected = done.rows_affected();
        if affected != statement.expected_rows {
            let _ = tx.rollback().await;
            return Err((
                Some(index),
                DbError::row_count_mismatch(affected, statement.expected_rows),
            ));
        }
    }
    tx.commit()
        .await
        .map_err(|e| (None, DbError::Query(e.to_string())))
}

type SqliteQuery<'q> = sqlx::query::Query<'q, sqlx::Sqlite, sqlx::sqlite::SqliteArguments<'q>>;

/// Binds backend-neutral [`Value`] parameters onto a prepared query.
fn bind_params<'q>(mut query: SqliteQuery<'q>, params: &[Value]) -> SqliteQuery<'q> {
    for param in params {
        query = match param {
            Value::Null => query.bind(None::<i64>),
            Value::Integer(i) => query.bind(*i),
            Value::Real(r) => query.bind(*r),
            Value::Text(t) => query.bind(t.clone()),
            Value::Blob(b) => query.bind(b.clone()),
        };
    }
    query
}

/// Like [`query`], with bound parameters (used by the paged table reader so
/// filter values never touch the SQL text).
///
/// No header recovery on a zero-row result here, unlike [`query`] and
/// [`query_capped`]: every caller of this builds its own projection (a page
/// fetch, a cell fetch, a catalog read for DDL) and so already knows the
/// columns, and nothing downstream reads them off the result. Describing the
/// statement to fill in headers no one looks at would put an extra round trip
/// on each of those — two on Postgres, whose `describe` also issues a
/// `pg_attribute` query — and empty catalog reads are ordinary (a table with
/// no table-level constraints, no non-constraint indexes).
pub async fn query_with(
    pool: &SqlitePool,
    sql: &str,
    params: &[Value],
) -> Result<QueryResult, DbError> {
    let stream = bind_params(sqlx::query(sql), params).fetch(pool);
    sqlx_common::collect_all(stream, decode_value, query_error).await
}

/// Streams `sql` one row at a time (`fetch`, not `fetch_all`), decoding and
/// retaining at most `max_rows` rows and capping each cell to `cell_cap`
/// bytes, so neither the row count nor a pathologically large cell can make
/// the free-form query path scale with table or value size. Returns
/// the (bounded) result and whether more rows existed beyond the cap.
pub async fn query_capped(
    pool: &SqlitePool,
    sql: &str,
    params: &[Value],
    max_rows: u64,
    cell_cap: usize,
) -> Result<(QueryResult, bool), DbError> {
    let stream = bind_params(sqlx::query(sql), params).fetch(pool);
    let (mut result, truncated) =
        sqlx_common::collect_capped(stream, max_rows, cell_cap, decode_value, query_error).await?;
    sqlx_common::fill_headers(&mut result, pool, sql).await;
    Ok((result, truncated))
}

/// [`query_capped`] against a single connection (e.g. one borrowed from a
/// transaction) rather than the pool — the read path for statements inside an
/// atomically-wrapped script. No bound params: scripts are raw text.
pub async fn query_capped_conn(
    conn: &mut sqlx::sqlite::SqliteConnection,
    sql: &str,
    max_rows: u64,
    cell_cap: usize,
) -> Result<(QueryResult, bool), DbError> {
    let stream = sqlx::query(sql).fetch(&mut *conn);
    let (mut result, truncated) =
        sqlx_common::collect_capped(stream, max_rows, cell_cap, decode_value, query_error).await?;
    sqlx_common::fill_headers(&mut result, &mut *conn, sql).await;
    Ok((result, truncated))
}

/// Runs a non-row statement on a single connection (e.g. one borrowed from a
/// transaction) rather than the pool, returning affected rows — the write
/// path for statements inside an atomically-wrapped script.
pub async fn execute_conn(
    conn: &mut sqlx::sqlite::SqliteConnection,
    sql: &str,
) -> Result<u64, DbError> {
    sqlx_common::execute(conn, sql, query_error).await
}

/// [`execute_conn`] with bound parameters — the write path for a batch of
/// imported rows, which must run inside the caller's transaction
/// and must not interpolate its values into the SQL.
pub async fn execute_with_conn(
    conn: &mut sqlx::sqlite::SqliteConnection,
    sql: &str,
    params: &[Value],
) -> Result<u64, DbError> {
    bind_params(sqlx::query(sql), params)
        .execute(conn)
        .await
        .map(|done| done.rows_affected())
        .map_err(query_error)
}

/// Streams a query to `out` in the given format, pulling rows one at a time
/// (`fetch`, not `fetch_all`) and writing each incrementally — peak memory is
/// one decoded row plus the writer's buffer. Returns the number of data rows
/// written. When the result is empty the column names come from a statement
/// describe so the header (CSV) / empty array (JSON) still reflect the query.
pub async fn export(
    pool: &SqlitePool,
    sql: &str,
    params: &[Value],
    format: ExportFormat,
    out: &mut impl Write,
) -> Result<u64, DbError> {
    let stream = bind_params(sqlx::query(sql), params).fetch(pool);
    if let Some(rows) =
        sqlx_common::export_stream(stream, format, out, decode_value, query_error).await?
    {
        return Ok(rows);
    }
    // No rows streamed, so no row carried the column names: describe the
    // statement instead. This one propagates its failure — an export whose
    // header cannot be determined is an empty file that says nothing.
    let columns = sqlx_common::describe_columns(pool, sql)
        .await
        .map_err(query_error)?;
    sqlx_common::export_empty(format, columns, out)?;
    Ok(0)
}

/// Lists tables and views with columns, indexes, and foreign keys: five
/// queries, plus one per view or virtual table — see the module header
/// for the pragma table-valued-function strategy and [`table_columns`] for the
/// exception.
pub async fn introspect(pool: &SqlitePool) -> Result<Vec<TableMeta>, DbError> {
    let sql = format!(
        "SELECT m.name AS name, m.type AS type, m.rootpage AS rootpage \
         FROM sqlite_master m \
         WHERE {MASTER_FILTER} \
         ORDER BY m.name"
    );
    let rows = sqlx::query(&sql)
        .fetch_all(pool)
        .await
        .map_err(|e| DbError::Introspect(e.to_string()))?;
    let mut entries = Vec::with_capacity(rows.len());
    for row in rows {
        let kind: String = get(&row, "type")?;
        let rootpage: Option<i64> = get(&row, "rootpage")?;
        entries.push(MasterEntry {
            name: get(&row, "name")?,
            virtual_table: is_virtual_table(&kind, rootpage),
            view: kind == "view",
        });
    }

    let (mut columns, unreadable) = table_columns(pool, &entries).await?;
    let mut indexes = table_indexes(pool).await?;
    let mut foreign_keys = table_foreign_keys(pool).await?;

    let mut tables = Vec::with_capacity(entries.len());
    for entry in entries {
        let restriction = restriction_for(&entry, !unreadable.contains(&entry.name));
        let MasterEntry { name, view, .. } = entry;
        tables.push(TableMeta {
            schema: None,
            columns: columns.remove(&name).unwrap_or_default(),
            indexes: indexes.remove(&name).unwrap_or_default(),
            foreign_keys: foreign_keys.remove(&name).unwrap_or_default(),
            name,
            kind: if view {
                TableKind::View
            } else {
                TableKind::Table
            },
            restriction,
            // SQLite has no extension-owned schemas and no declarative
            // partitioning; its extensions contribute functions and virtual
            // tables, which are the user's to use.
            internal: None,
            kind_label: None,
        });
    }
    Ok(tables)
}

/// One object the schema tree lists, as `sqlite_master` describes it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct MasterEntry {
    name: String,
    view: bool,
    /// A `CREATE VIRTUAL TABLE` object — see [`is_virtual_table`].
    virtual_table: bool,
}

/// Whether a `sqlite_master` row is a virtual table.
///
/// Decided on the catalog's own fact rather than the stored statement's text:
/// a virtual table is a `table` row with no b-tree behind it, so its
/// `rootpage` is 0 (the file format documents "0 or NULL" for views,
/// triggers and virtual tables). Every ordinary table — `WITHOUT ROWID`
/// included — owns a root page.
fn is_virtual_table(kind: &str, rootpage: Option<i64>) -> bool {
    kind == "table" && rootpage.unwrap_or(0) == 0
}

/// Why a virtual table whose columns SQLite could not read is not opened.
const UNREADABLE_VIRTUAL_TABLE: &str =
    "SQLite can't open this virtual table here — usually because its module \
     comes from an extension (SpatiaLite, sqlean, …) that hubro doesn't load.";

/// Why any other object whose columns SQLite could not read is not opened.
const UNREADABLE_OBJECT: &str = "SQLite can't read this object — its definition refers to \
     a table, column, function, collation or virtual-table module that isn't available.";

/// The restriction introspection records for one object.
///
/// `None` for everything SQLite could read the columns of: kind and row
/// identity already carry everything this backend knows about writability.
/// An object it could *not* read — a virtual table whose module hubro's
/// SQLite lacks, a view over one, a view calling an extension's function
/// (SpatiaLite's `ST_*`, sqlean's) or naming a missing collation, a view over
/// a table that has since been dropped — is still listed, so one such object
/// costs the user that object rather than the whole schema tree, but as
/// `NoRows`: opening it can only fail the same way, so the grid states why
/// instead of running the query.
/// The DDL view still works on it, since that reads only `sqlite_master`.
fn restriction_for(entry: &MasterEntry, columns_readable: bool) -> Option<Restriction> {
    if columns_readable {
        None
    } else if entry.virtual_table {
        Some(Restriction::NoRows(UNREADABLE_VIRTUAL_TABLE))
    } else {
        Some(Restriction::NoRows(UNREADABLE_OBJECT))
    }
}

/// The `sqlite_master` filter the object list in [`introspect`] and every
/// batched pragma join below share.
///
/// The `sqlite_` prefix is matched literally — `_` escaped, since unescaped it
/// matches any one character and hid user tables such as `sqlitest` and
/// `sqlite-log`. `LIKE` stays case-insensitive on purpose: SQLite reserves the
/// prefix in any case (`CREATE TABLE SQLITE_x` is refused as "reserved for
/// internal use"), so this hides exactly what SQLite itself owns — including
/// a mixed-case name forged in under `writable_schema`, which is the only way
/// one gets there.
const MASTER_FILTER: &str =
    "m.type IN ('table', 'view') AND m.name NOT LIKE 'sqlite\\_%' ESCAPE '\\'";

/// Whether a failed catalog read is SQLite's generic `SQLITE_ERROR` — what it
/// raises when resolving one object fails: a virtual table's module is missing
/// (`no such module: …`), or a view refers to a table, column, function or
/// collation that isn't there (`no such table: …`, `no such function: …`).
///
/// Decided on the result code, never the message. `BUSY`, `LOCKED`, `IOERR`,
/// `CORRUPT` and the rest say the database cannot be read at all, and
/// recording that as "this object is unreadable" would turn a failure the user
/// should see into a tree of dimmed objects. sqlx reports the *extended*
/// code, whose low byte is the primary one.
fn is_object_scoped(extended_code: Option<&str>) -> bool {
    const SQLITE_ERROR: i64 = 1;
    extended_code
        .and_then(|code| code.parse::<i64>().ok())
        .is_some_and(|code| code & 0xff == SQLITE_ERROR)
}

/// Whether reading this object's columns makes SQLite *resolve* something —
/// compile a view's `SELECT`, connect a virtual table to its module — and so
/// can fail for a reason that belongs to this object alone.
///
/// An ordinary table never can: SQLite answers `pragma_table_xinfo` for one
/// from the parsed schema without resolving anything
/// (`sqlite3ViewGetColumnNames` returns before doing any work for a
/// non-virtual table), so a failure reading one is about the database or about
/// hubro's query, never about that table.
fn needs_resolving(entry: &MasterEntry) -> bool {
    entry.view || entry.virtual_table
}

/// Whether a failure reading `entry`'s columns is that object's own — the one
/// decision that turns an error into a dimmed entry instead of a failed
/// schema tree. Both halves must hold: the object is one whose read resolves
/// something ([`needs_resolving`]), and the failure is the generic error that
/// resolving raises ([`is_object_scoped`]).
fn charged_to_object(entry: &MasterEntry, err: &sqlx::Error) -> bool {
    needs_resolving(entry)
        && match err {
            sqlx::Error::Database(db) => is_object_scoped(db.code().as_deref()),
            _ => false,
        }
}

/// Columns keyed by object name, and the names of the objects whose columns
/// SQLite could not read.
type ColumnsRead = (HashMap<String, Vec<ColumnMeta>>, HashSet<String>);

/// Why reading one object's columns failed: the driver's error, kept whole so
/// [`charged_to_object`] can read its result code, or a row that would not
/// decode, which is never the object's fault.
#[derive(Debug)]
enum ColumnReadError {
    Driver(sqlx::Error),
    Decode(DbError),
}

/// The rows the batched query reads: ordinary tables, i.e. `!needs_resolving`
/// in SQL (see [`needs_resolving`]). Everything else is read one query each by
/// [`read_each`] — every table and view has at least one column, so an object
/// missing from the batch's result is one it did not read.
const ORDINARY_TABLE: &str = "m.type = 'table' AND m.rootpage <> 0";

/// Columns of every table and view, keyed by object name (unique per SQLite
/// schema), plus the names of the objects whose columns SQLite could not read.
///
/// `table_xinfo` (not `table_info`) so generated (`AS (…)`) columns are
/// reported: they appear in `SELECT *`, so the grid's bounded page fetch —
/// which builds an explicit projection from this metadata — must know about
/// them, and they must be read-only.
///
/// Ordinary tables are read in one batched query. Views and virtual tables
/// are read one query each, because `pragma_table_xinfo` has to resolve them,
/// and in a batch one that cannot be resolved fails the whole statement —
/// which used to empty the schema tree for a database holding a single
/// SpatiaLite table, or a view over a dropped table. Read alone, such an
/// object costs only itself (see [`read_each`]). The index and foreign-key
/// pragmas need none of this: they read the schema without resolving anything
/// (measured — both answer an empty list for such an object), which the tests
/// over such a database hold them to.
async fn table_columns(pool: &SqlitePool, entries: &[MasterEntry]) -> Result<ColumnsRead, DbError> {
    let sql = format!(
        "SELECT m.name AS table_name, {XINFO_COLUMNS} \
         FROM sqlite_master m JOIN pragma_table_xinfo(m.name) x \
         WHERE {MASTER_FILTER} AND {ORDINARY_TABLE} \
         ORDER BY m.name, x.cid"
    );
    let rows = sqlx::query(&sql)
        .fetch_all(pool)
        .await
        .map_err(|e| DbError::Introspect(e.to_string()))?;
    let mut columns: HashMap<String, Vec<ColumnMeta>> = HashMap::new();
    for row in rows {
        let table: String = get(&row, "table_name")?;
        if let Some(column) = column_meta(&row)? {
            columns.entry(table).or_default().push(column);
        }
    }

    let pending: Vec<&MasterEntry> = entries
        .iter()
        .filter(|entry| !columns.contains_key(&entry.name))
        .collect();
    let sql = format!("SELECT {XINFO_COLUMNS} FROM pragma_table_xinfo(?1) x ORDER BY x.cid");
    let sql = sql.as_str();
    let (resolved, unreadable) = read_each(&pending, |name| async move {
        let rows = sqlx::query(sql)
            .bind(name)
            .fetch_all(pool)
            .await
            .map_err(ColumnReadError::Driver)?;
        let mut list = Vec::with_capacity(rows.len());
        for row in &rows {
            list.extend(column_meta(row).map_err(ColumnReadError::Decode)?);
        }
        Ok(list)
    })
    .await?;
    columns.extend(resolved);
    Ok((columns, unreadable))
}

/// Reads each entry's columns through `read`, one object at a time, recording
/// the objects whose failure is their own ([`charged_to_object`]) as
/// unreadable and carrying on. Any other failure fails the whole read.
///
/// Takes the read as a parameter so the decision can be run against failures
/// a real database will not produce on demand — a `BUSY` or an `IOERR`
/// mid-loop must surface as an error, not as one more dimmed object.
async fn read_each<F, Fut>(entries: &[&MasterEntry], mut read: F) -> Result<ColumnsRead, DbError>
where
    F: FnMut(String) -> Fut,
    Fut: std::future::Future<Output = Result<Vec<ColumnMeta>, ColumnReadError>>,
{
    let mut columns = HashMap::new();
    let mut unreadable = HashSet::new();
    for entry in entries {
        match read(entry.name.clone()).await {
            Ok(list) => {
                columns.insert(entry.name.clone(), list);
            }
            Err(ColumnReadError::Driver(err)) if charged_to_object(entry, &err) => {
                unreadable.insert(entry.name.clone());
            }
            Err(ColumnReadError::Driver(err)) => return Err(DbError::Introspect(err.to_string())),
            Err(ColumnReadError::Decode(err)) => return Err(err),
        }
    }
    Ok((columns, unreadable))
}

/// The `pragma_table_xinfo` columns [`column_meta`] decodes, from the pragma
/// aliased as `x` — shared by the batched and the per-object read so the two
/// cannot decode different shapes.
const XINFO_COLUMNS: &str = "x.name AS name, x.type AS type, \
     x.\"notnull\" AS \"notnull\", x.dflt_value AS dflt_value, \
     x.pk AS pk, x.hidden AS hidden";

/// One `pragma_table_xinfo` row as a column, or `None` for a column `SELECT *`
/// does not return.
///
/// `table_xinfo` adds a `hidden` column: 0 = ordinary, 2 = VIRTUAL generated,
/// 3 = STORED generated, 1 = a hidden column (virtual-table machinery) that
/// `SELECT *` does NOT return — skipped so the projection still matches
/// `SELECT *`.
fn column_meta(row: &SqliteRow) -> Result<Option<ColumnMeta>, DbError> {
    let hidden: i64 = get(row, "hidden")?;
    if hidden == 1 {
        return Ok(None);
    }
    let pk: i64 = get(row, "pk")?;
    let notnull: i64 = get(row, "notnull")?;
    Ok(Some(ColumnMeta {
        name: get(row, "name")?,
        type_name: get(row, "type")?,
        nullable: notnull == 0,
        primary_key_position: (pk > 0).then_some(pk as u32),
        default: get::<Option<String>, _>(row, "dflt_value")?,
        // 2/3 are VIRTUAL/STORED generated columns: database-assigned and
        // not writable through ordinary INSERT/UPDATE.
        generated: if hidden == 2 || hidden == 3 {
            Generated::Always
        } else {
            Generated::Never
        },
        // SQLite has no enum or array types.
        type_detail: TypeDetail::Plain,
    }))
}

/// Indexes of every table, keyed by table name. Two queries: the per-table
/// index list, and every index's key columns keyed by index name — index
/// names are unique across a SQLite schema, so one flat map covers all
/// tables.
async fn table_indexes(pool: &SqlitePool) -> Result<HashMap<String, Vec<IndexMeta>>, DbError> {
    let sql = format!(
        "SELECT il.name AS index_name, ii.seqno AS seqno, ii.name AS column_name \
         FROM sqlite_master m \
         JOIN pragma_index_list(m.name) il \
         JOIN pragma_index_info(il.name) ii \
         WHERE {MASTER_FILTER}"
    );
    let column_rows = sqlx::query(&sql)
        .fetch_all(pool)
        .await
        .map_err(|e| DbError::Introspect(e.to_string()))?;
    let mut index_columns: HashMap<String, Vec<(i64, Option<String>)>> = HashMap::new();
    for col in column_rows {
        let index: String = get(&col, "index_name")?;
        index_columns
            .entry(index)
            .or_default()
            .push((get(&col, "seqno")?, get(&col, "column_name")?));
    }

    // `seq` is the pragma's own output ordinal, so ordering by it keeps each
    // table's indexes in the order the per-table pragma reported them.
    let sql = format!(
        "SELECT m.name AS table_name, il.name AS index_name, \
                il.\"unique\" AS \"unique\", il.partial AS partial \
         FROM sqlite_master m JOIN pragma_index_list(m.name) il \
         WHERE {MASTER_FILTER} \
         ORDER BY m.name, il.seq"
    );
    let rows = sqlx::query(&sql)
        .fetch_all(pool)
        .await
        .map_err(|e| DbError::Introspect(e.to_string()))?;
    let mut indexes: HashMap<String, Vec<IndexMeta>> = HashMap::new();
    for row in rows {
        let table: String = get(&row, "table_name")?;
        let name: String = get(&row, "index_name")?;
        let unique: i64 = get(&row, "unique")?;
        let partial: i64 = get(&row, "partial")?;
        let mut columns = index_columns.remove(&name).unwrap_or_default();
        columns.sort_by_key(|(seqno, _)| *seqno);
        indexes.entry(table).or_default().push(IndexMeta {
            name,
            unique: unique != 0,
            partial: partial != 0,
            // A NULL column name means the index is on an expression or rowid.
            columns: columns
                .into_iter()
                .map(|(_, name)| name.unwrap_or_else(|| "<expr>".to_string()))
                .collect(),
        });
    }
    Ok(indexes)
}

/// One `pragma_foreign_key_list` row: (id, seq, referenced table, from, to).
type FkPart = (i64, i64, String, String, Option<String>);

/// Foreign keys of every table, keyed by table name.
async fn table_foreign_keys(
    pool: &SqlitePool,
) -> Result<HashMap<String, Vec<ForeignKeyMeta>>, DbError> {
    let sql = format!(
        "SELECT m.name AS table_name, fk.id AS id, fk.seq AS seq, \
                fk.\"table\" AS ref_table, fk.\"from\" AS from_col, fk.\"to\" AS to_col \
         FROM sqlite_master m JOIN pragma_foreign_key_list(m.name) fk \
         WHERE {MASTER_FILTER}"
    );
    let rows = sqlx::query(&sql)
        .fetch_all(pool)
        .await
        .map_err(|e| DbError::Introspect(e.to_string()))?;
    // Rows arrive one per column, keyed by (id, seq); group them into FKs.
    let mut parts: HashMap<String, Vec<FkPart>> = HashMap::new();
    for row in rows {
        let table: String = get(&row, "table_name")?;
        parts.entry(table).or_default().push((
            get(&row, "id")?,
            get(&row, "seq")?,
            get(&row, "ref_table")?,
            get(&row, "from_col")?,
            get(&row, "to_col")?,
        ));
    }

    let mut foreign_keys: HashMap<String, Vec<ForeignKeyMeta>> =
        HashMap::with_capacity(parts.len());
    for (table, mut parts) in parts {
        parts.sort_by_key(|(id, seq, ..)| (*id, *seq));
        let mut fks: Vec<(i64, ForeignKeyMeta)> = Vec::new();
        for (id, _, referenced_table, from, to) in parts {
            match fks.last_mut() {
                Some((last_id, fk)) if *last_id == id => {
                    fk.columns.push(from);
                    fk.referenced_columns.push(to);
                }
                _ => fks.push((
                    id,
                    ForeignKeyMeta {
                        columns: vec![from],
                        referenced_schema: None,
                        referenced_table,
                        referenced_columns: vec![to],
                    },
                )),
            }
        }
        foreign_keys.insert(table, fks.into_iter().map(|(_, fk)| fk).collect());
    }
    Ok(foreign_keys)
}

/// Row count and on-disk size for one table — of which SQLite can
/// cheaply answer only the first, and only sometimes.
///
/// **Rows** come from `sqlite_stat1`, the table `ANALYZE` writes: the first
/// whitespace-separated token of each `stat` string is that table's row count
/// at the time it ran. SQLite keeps no statistics otherwise, so a database
/// nobody has analyzed reports nothing here — correctly, and the exact count is
/// one click away. The table itself does not exist until the first `ANALYZE`,
/// so its presence is checked in `sqlite_master` first: selecting from a
/// missing table is an error, and "this database has no statistics" is not one.
///
/// **Size is deliberately absent.** SQLite's only per-table size is the
/// `dbstat` virtual table, which computes its answer by walking every page of
/// the b-trees it reports on — the same order of cost as the `COUNT(*)` this
/// whole path exists to avoid, and paid on merely opening the schema pane. The
/// cheap alternatives (`PRAGMA page_count`) measure the *file*, not the table,
/// and reporting a database's size as a table's would be a wrong number rather
/// than a missing one.
pub async fn fetch_table_stats(
    pool: &SqlitePool,
    table: &TableMeta,
) -> Result<TableStats, DbError> {
    let analyzed: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name = 'sqlite_stat1'",
    )
    .fetch_one(pool)
    .await
    .map_err(|e| DbError::Introspect(e.to_string()))?;
    if analyzed == 0 {
        return Ok(TableStats::default());
    }
    // Any of the table's rows will do: ANALYZE writes one per index, plus one
    // with a NULL `idx` when there is no index, and every one of them opens
    // with the table's own row count.
    let stat: Option<String> =
        sqlx::query_scalar("SELECT stat FROM sqlite_stat1 WHERE tbl = ?1 AND stat IS NOT NULL")
            .bind(&table.name)
            .fetch_optional(pool)
            .await
            .map_err(|e| DbError::Introspect(e.to_string()))?;
    Ok(TableStats {
        rows: stat
            .as_deref()
            .and_then(sqlite_stat_rows)
            .map(RowCount::Estimated),
        bytes: None,
    })
}

/// The row count leading a `sqlite_stat1.stat` string ("10000 500 1" → 10000),
/// or `None` if it does not lead with one.
///
/// The count always comes first: `ANALYZE` writes the table's row count, then
/// the per-column selectivities, and the non-numeric hints (`unordered`,
/// `sz=…`, `noskipscan`) are *appended* after that list rather than prefixed.
///
/// Parsed defensively anyway, because `sqlite_stat1` is an ordinary
/// user-writable table and hand-editing it to steer the planner is a
/// documented technique — so its contents are input, not a guarantee.
/// Anything that does not lead with a number is unknown, which renders as
/// nothing rather than as a guess.
fn sqlite_stat_rows(stat: &str) -> Option<u64> {
    stat.split_whitespace().next()?.parse().ok()
}

/// DDL for one object. SQLite stores the original `CREATE`
/// statement text in `sqlite_master`, so tables, views, and explicitly
/// created indexes all come back exactly as they were written — no
/// reconstruction, no header. The one exception is an index SQLite created
/// itself to back a `UNIQUE`/`PRIMARY KEY` constraint: those have a NULL
/// `sqlite_master.sql`, because the constraint in the table definition *is*
/// their definition.
pub async fn fetch_ddl(
    pool: &SqlitePool,
    table: &TableMeta,
    object: &DdlObject,
) -> Result<Ddl, DbError> {
    match object {
        DdlObject::Index(name) => {
            match object_sql(pool, name, &["index"]).await? {
                Some(Some(sql)) => Ok(Ddl::native(terminate(&sql, Dialect::Sqlite))),
                // No such index in the database, whatever the (possibly
                // stale) metadata says: dropped since the tree was read. Only
                // the catalog row can tell this from the implicit index below,
                // which also has no stored text.
                None => Err(DbError::Introspect(format!("no index named {name}"))),
                // An implicit constraint index. Rebuilding it as a standalone
                // CREATE INDEX would be a different object from what is
                // there, so the caveat says where the real definition lives.
                Some(None) => {
                    let Some(index) = table.indexes.iter().find(|i| &i.name == name) else {
                        return Err(DbError::Introspect(format!("no index named {name}")));
                    };
                    let mut ddl =
                        create_index_sql(Dialect::Sqlite, table, index, &IndexExtras::default());
                    ddl.caveats.insert(
                        0,
                        "SQLite created this index for a UNIQUE / PRIMARY KEY constraint and \
                         stores no CREATE statement for it — the table definition is its source"
                            .into(),
                    );
                    Ok(ddl)
                }
            }
        }
        DdlObject::Object => {
            let Some(Some(sql)) = object_sql(pool, &table.name, &["table", "view"]).await? else {
                return Err(DbError::Introspect(format!(
                    "no stored definition for {}",
                    table.name
                )));
            };
            let mut out = terminate(&sql, Dialect::Sqlite);
            // A table's own indexes belong with it: showing the CREATE TABLE
            // alone would hand the user a statement that silently loses them.
            // Constraint-backed indexes have a NULL sql and are already part
            // of the table text, so they drop out here on their own.
            for index in index_sql(pool, &table.name).await? {
                out.push_str("\n\n");
                out.push_str(&terminate(&index, Dialect::Sqlite));
            }
            Ok(Ddl::native(out))
        }
    }
}

/// The stored `CREATE` text of one `sqlite_master` object of one of `kinds`.
///
/// Two layers of `Option` because the two absences mean different things:
/// `None` is no such object (never created, or dropped since the metadata was
/// read — a row of another kind counts, since tables, views and indexes share
/// one namespace), and `Some(None)` is an object whose row has a NULL `sql`,
/// which for an index means SQLite created it itself for a constraint.
/// Flattening them once made a dropped index show up as a reconstructed
/// constraint index.
async fn object_sql(
    pool: &SqlitePool,
    name: &str,
    kinds: &[&str],
) -> Result<Option<Option<String>>, DbError> {
    // `type` is matched in Rust rather than through a bound IN-list: sqlx has
    // no array binding on SQLite, and the kind set is a fixed literal here.
    // Triggers are excluded because they are the one kind with a namespace of
    // their own: a trigger may share a name with an index or table, and
    // whichever row came first would otherwise answer for both.
    let row =
        sqlx::query("SELECT type, sql FROM sqlite_master WHERE name = ?1 AND type <> 'trigger'")
            .bind(name)
            .fetch_optional(pool)
            .await
            .map_err(|e| DbError::Introspect(e.to_string()))?;
    let Some(row) = row else {
        return Ok(None);
    };
    let kind: String = get(&row, "type")?;
    if !kinds.contains(&kind.as_str()) {
        return Ok(None);
    }
    get::<Option<String>, _>(&row, "sql").map(Some)
}

/// The stored `CREATE INDEX` statements of every explicitly created index on
/// `table`, in name order.
async fn index_sql(pool: &SqlitePool, table: &str) -> Result<Vec<String>, DbError> {
    let rows = sqlx::query(
        "SELECT sql FROM sqlite_master \
         WHERE type = 'index' AND tbl_name = ?1 AND sql IS NOT NULL \
         ORDER BY name",
    )
    .bind(table)
    .fetch_all(pool)
    .await
    .map_err(|e| DbError::Introspect(e.to_string()))?;
    rows.iter().map(|row| get(row, "sql")).collect()
}

fn decode_value(row: &SqliteRow, idx: usize) -> Result<Value, DbError> {
    let raw = row
        .try_get_raw(idx)
        .map_err(|e| DbError::Query(e.to_string()))?;
    if raw.is_null() {
        return Ok(Value::Null);
    }
    let decoded = match raw.type_info().name() {
        "INTEGER" | "BOOLEAN" => Value::Integer(
            row.try_get::<i64, _>(idx)
                .map_err(|e| DbError::Query(e.to_string()))?,
        ),
        "REAL" => Value::Real(
            row.try_get::<f64, _>(idx)
                .map_err(|e| DbError::Query(e.to_string()))?,
        ),
        "BLOB" => Value::Blob(
            row.try_get::<Vec<u8>, _>(idx)
                .map_err(|e| DbError::Query(e.to_string()))?,
        ),
        _ => Value::Text(
            row.try_get::<String, _>(idx)
                .map_err(|e| DbError::Query(e.to_string()))?,
        ),
    };
    Ok(decoded)
}

#[cfg(test)]
mod tests {
    use super::super::sql::quote_ident;
    use super::*;

    #[test]
    fn quote_ident_escapes_embedded_quotes() {
        assert_eq!(quote_ident("plain"), "\"plain\"");
        assert_eq!(quote_ident("we\"ird"), "\"we\"\"ird\"");
        assert_eq!(quote_ident("with space"), "\"with space\"");
    }

    #[test]
    fn only_a_generic_error_is_charged_to_one_object() {
        // SQLITE_ERROR: what `no such module` and `no such table` raise, and
        // its extended forms (MISSING_COLLSEQ = 257).
        assert!(is_object_scoped(Some("1")));
        assert!(is_object_scoped(Some("257")));
        // Everything that says the database itself can't be read stays a
        // failure of the whole read: BUSY, BUSY_SNAPSHOT, LOCKED, NOMEM,
        // IOERR_READ, CORRUPT, NOTADB.
        for code in ["5", "517", "6", "7", "266", "11", "26"] {
            assert!(!is_object_scoped(Some(code)), "{code}");
        }
        // No code, or one that isn't a number, is not evidence of anything.
        assert!(!is_object_scoped(None));
        assert!(!is_object_scoped(Some("HY000")));
    }

    /// A driver error carrying a chosen SQLite result code — what the per-object
    /// read meets from a real database, without needing one that is busy or
    /// failing I/O on demand.
    #[derive(Debug)]
    struct FakeSqliteError(&'static str);

    impl std::fmt::Display for FakeSqliteError {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "fake sqlite error {}", self.0)
        }
    }

    impl std::error::Error for FakeSqliteError {}

    impl sqlx::error::DatabaseError for FakeSqliteError {
        fn message(&self) -> &str {
            "fake"
        }
        fn code(&self) -> Option<std::borrow::Cow<'_, str>> {
            Some(self.0.into())
        }
        fn as_error(&self) -> &(dyn std::error::Error + Send + Sync + 'static) {
            self
        }
        fn as_error_mut(&mut self) -> &mut (dyn std::error::Error + Send + Sync + 'static) {
            self
        }
        fn into_error(self: Box<Self>) -> Box<dyn std::error::Error + Send + Sync + 'static> {
            self
        }
        fn kind(&self) -> sqlx::error::ErrorKind {
            sqlx::error::ErrorKind::Other
        }
    }

    fn db_error(code: &'static str) -> ColumnReadError {
        ColumnReadError::Driver(sqlx::Error::Database(Box::new(FakeSqliteError(code))))
    }

    fn entry(name: &str, view: bool, virtual_table: bool) -> MasterEntry {
        MasterEntry {
            name: name.into(),
            view,
            virtual_table,
        }
    }

    fn one_column() -> Vec<ColumnMeta> {
        vec![ColumnMeta {
            name: "c".into(),
            type_name: "TEXT".into(),
            nullable: true,
            primary_key_position: None,
            default: None,
            generated: Generated::Never,
            type_detail: TypeDetail::Plain,
        }]
    }

    /// Runs [`read_each`] over `entries`, failing the read of `failing` with
    /// whatever `fail` builds and answering one column for everything else.
    fn read_with_failure(
        entries: &[MasterEntry],
        failing: &str,
        fail: impl Fn() -> ColumnReadError,
    ) -> Result<ColumnsRead, DbError> {
        let refs: Vec<&MasterEntry> = entries.iter().collect();
        let read = |name: String| {
            let result = if name == failing {
                Err(fail())
            } else {
                Ok(one_column())
            };
            std::future::ready(result)
        };
        tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .block_on(read_each(&refs, read))
    }

    #[test]
    fn a_view_or_virtual_table_that_cannot_be_resolved_costs_only_itself() {
        // The failing object comes first, so carrying on past it is tested
        // too, not just recording it.
        for failing in [entry("v", true, false), entry("z", false, true)] {
            let entries = [
                failing.clone(),
                entry("t", false, false),
                entry("w", true, false),
            ];
            let (columns, unreadable) =
                read_with_failure(&entries, &failing.name, || db_error("1")).unwrap();
            assert_eq!(unreadable, HashSet::from([failing.name.clone()]));
            assert!(!columns.contains_key(&failing.name));
            assert!(columns.contains_key("t") && columns.contains_key("w"));
        }
    }

    #[test]
    fn a_failure_that_is_not_the_objects_own_fails_the_whole_read() {
        let view = [entry("v", true, false), entry("t", false, false)];
        // The database itself can't be read: BUSY, CORRUPT, IOERR_READ.
        for code in ["5", "11", "266"] {
            assert!(
                read_with_failure(&view, "v", || db_error(code)).is_err(),
                "code {code} was charged to the view"
            );
        }
        // Not a database error at all.
        assert!(read_with_failure(&view, "v", || ColumnReadError::Driver(
            sqlx::Error::PoolTimedOut
        ))
        .is_err());
        // A row that would not decode.
        assert!(
            read_with_failure(&view, "v", || ColumnReadError::Decode(DbError::Introspect(
                "bad row".into()
            )))
            .is_err()
        );
        // And an ordinary table resolves nothing, so even the generic error is
        // not its own: something is wrong with the read, not the table.
        let table = [entry("t", false, false), entry("v", true, false)];
        assert!(read_with_failure(&table, "t", || db_error("1")).is_err());
    }

    #[test]
    fn a_virtual_table_is_a_table_without_a_root_page() {
        assert!(is_virtual_table("table", Some(0)));
        assert!(is_virtual_table("table", None));
        assert!(!is_virtual_table("table", Some(2)));
        // Views have no root page either, and are not virtual tables.
        assert!(!is_virtual_table("view", Some(0)));
    }

    #[test]
    fn only_an_object_whose_columns_could_not_be_read_is_restricted() {
        let entry = |view, virtual_table| MasterEntry {
            name: "x".into(),
            view,
            virtual_table,
        };
        // Readable: nothing to declare, whatever the object is — a virtual
        // table with its module present included.
        for (view, virtual_table) in [(false, false), (true, false), (false, true)] {
            assert_eq!(restriction_for(&entry(view, virtual_table), true), None);
        }
        // Unreadable: not openable, with the reason matching what it is.
        assert_eq!(
            restriction_for(&entry(false, true), false),
            Some(Restriction::NoRows(UNREADABLE_VIRTUAL_TABLE))
        );
        for view in [true, false] {
            assert_eq!(
                restriction_for(&entry(view, false), false),
                Some(Restriction::NoRows(UNREADABLE_OBJECT))
            );
        }
        assert!(Restriction::NoRows(UNREADABLE_OBJECT).hides_rows());
    }

    #[test]
    fn a_stat1_row_count_is_read_only_when_it_really_leads_the_string() {
        use super::sqlite_stat_rows;
        // What ANALYZE writes: the table's rows, then per-column selectivity.
        assert_eq!(sqlite_stat_rows("10000 500 1"), Some(10_000));
        // A table with no index gets the bare count.
        assert_eq!(sqlite_stat_rows("7"), Some(7));
        assert_eq!(sqlite_stat_rows("0 1"), Some(0));
        // The hints are appended, so a real `stat` still leads with the count.
        assert_eq!(sqlite_stat_rows("120 1 unordered"), Some(120));
        // And anything hand-written that does not lead with a number is
        // unknown rather than guessed at.
        assert_eq!(sqlite_stat_rows("unordered 42"), None);
        assert_eq!(sqlite_stat_rows(""), None);
        assert_eq!(sqlite_stat_rows("-3 1"), None);
    }

    #[test]
    fn the_file_check_names_which_problem_it_found() {
        let dir = tempfile::tempdir().unwrap();

        let missing = dir.path().join("nope.db");
        let err = check_sqlite_file(&missing).unwrap_err();
        assert_eq!(err.kind, SqliteFileErrorKind::NotFound);
        // The message names the file the user typed, so a mistyped path is
        // obvious from the message alone.
        assert!(err.to_string().contains("nope.db"), "{err}");
        assert!(err.to_string().starts_with("no such file"), "{err}");

        let err = check_sqlite_file(dir.path()).unwrap_err();
        assert_eq!(err.kind, SqliteFileErrorKind::NotAFile);

        let garbage = dir.path().join("holiday.jpg");
        std::fs::write(&garbage, b"\xff\xd8\xff\xe0 not a database at all").unwrap();
        let err = check_sqlite_file(&garbage).unwrap_err();
        assert_eq!(err.kind, SqliteFileErrorKind::NotADatabase);
        assert!(err.to_string().contains("not a SQLite database"), "{err}");
        // The point of the whole check: the driver's wording never reaches the
        // user for this case.
        assert!(!err.to_string().contains("code: 14"), "{err}");

        // Non-empty but too short to hold a header: still not a database.
        let stub = dir.path().join("stub.db");
        std::fs::write(&stub, b"SQLite").unwrap();
        assert_eq!(
            check_sqlite_file(&stub).unwrap_err().kind,
            SqliteFileErrorKind::NotADatabase
        );
    }

    #[test]
    fn the_file_check_accepts_what_sqlite_accepts() {
        let dir = tempfile::tempdir().unwrap();

        // A real header. (`open_sqlite` is exercised end-to-end against real
        // databases in tests/db_sqlite.rs; this pins the byte-level rule.)
        let real = dir.path().join("app.sqlite3");
        let mut bytes = SQLITE_HEADER.to_vec();
        bytes.extend_from_slice(&[0u8; 32]);
        std::fs::write(&real, &bytes).unwrap();
        assert!(check_sqlite_file(&real).is_ok());

        // A zero-length file is a valid empty database to SQLite itself —
        // `tests/db_sqlite.rs::open_empty_file_succeeds_like_sqlite_itself`
        // holds the driver to the same answer, so this is not just an
        // assumption about it.
        let empty = dir.path().join("empty.db");
        std::fs::write(&empty, b"").unwrap();
        assert!(check_sqlite_file(&empty).is_ok());
    }
}
