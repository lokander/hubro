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
//!
//! Every pragma is given the schema, `'main'`, which is what `sqlite_master`
//! lists. Unqualified, a pragma resolves its name temp-first, so a TEMP table
//! sharing a main table's name lent it its columns and indexes — and with them
//! the rowid-alias decision
//! (`a_temp_table_of_the_same_name_does_not_lend_its_shape_to_the_main_one`).

use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::path::Path;

use sqlx::sqlite::{
    SqliteConnectOptions, SqliteConnection, SqlitePool, SqlitePoolOptions, SqliteRow,
};
use sqlx::{Connection as _, Row as _, TypeInfo as _, ValueRef as _};

use super::caps::Restriction;
use super::ddl::{create_index_sql, terminate, Ddl, DdlObject, IndexExtras};
use super::error::DbError;
use super::export::ExportFormat;
use super::schema::{
    ColumnMeta, ForeignKeyMeta, Generated, IndexConstraint, IndexMeta, TableKind, TableMeta,
    TypeDetail,
};
use super::script::first_keyword;
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
    SqlText::new("PRAGMA schema_version")?
        .query()
        .fetch_one(&pool)
        .await
        .map_err(|e| DbError::Connect(e.to_string()))?;
    Ok(pool)
}

/// A new connection to `pool`'s database file, opened read-only
/// (`SQLITE_OPEN_READONLY`) — the engine-side half of a connection the user
/// marked read-only.
///
/// A flag on the *open*, not a pragma, because a pragma can be set back:
/// `PRAGMA query_only = 0` undoes `PRAGMA query_only = 1`, and sqlx runs every
/// statement in the string it is handed, so a lexer mismatch that hid a write
/// from the gate could hide the pragma in front of it just as well. Nothing a
/// statement can say reopens a connection read-write, and SQLite opens every
/// `ATTACH`ed database with the same flags.
///
/// A connection of its own rather than one of the pool's: the pool's
/// connections are the ones edits and imports write through, so they cannot
/// be read-only, and the marking can change while the tab is open.
pub async fn open_read_only(pool: &SqlitePool) -> Result<SqliteConnection, DbError> {
    let options = (*pool.connect_options()).clone().read_only(true);
    SqliteConnection::connect_with(&options)
        .await
        .map_err(|e| DbError::Connect(e.to_string()))
}

/// Runs an arbitrary query, decoding every cell into the backend-neutral
/// [`Value`] model. Free-form SQL whose result is shown to the user, so a
/// zero-row result recovers its headers.
pub async fn query(pool: &SqlitePool, sql: &str) -> Result<QueryResult, DbError> {
    sqlx_common::query_all::<SqliteBackend>(pool, SqlText::new(sql)?).await
}

/// The SQLite half of the generic sqlx paths ([`sqlx_common::SqlxBackend`]):
/// text reaches the driver as a [`SqlText`], so through the door.
pub(super) struct SqliteBackend;

impl sqlx_common::SqlxBackend for SqliteBackend {
    type DB = sqlx::Sqlite;
    type Text<'q> = SqlText<'q>;

    fn prepare(sql: &str) -> Result<SqlText<'_>, DbError> {
        SqlText::new(sql)
    }

    fn sql<'q>(text: Self::Text<'q>) -> &'q str {
        text.as_str()
    }

    fn query<'q>(text: Self::Text<'q>) -> SqliteQuery<'q> {
        text.query()
    }

    fn bind<'q>(query: SqliteQuery<'q>, value: &Value) -> SqliteQuery<'q> {
        match value {
            Value::Null => query.bind(None::<i64>),
            Value::Integer(i) => query.bind(*i),
            Value::Real(r) => query.bind(*r),
            Value::Text(t) => query.bind(t.clone()),
            Value::Blob(b) => query.bind(b.clone()),
        }
    }

    fn decode(row: &SqliteRow, idx: usize) -> Result<Value, DbError> {
        decode_value(row, idx)
    }

    fn map_err(err: sqlx::Error, _sql: &str) -> DbError {
        query_error(err)
    }
}

/// Maps a driver error onto [`DbError::Query`]. sqlx 0.8 does not expose
/// `sqlite3_error_offset` (its `SqliteError` carries only code + message), so
/// failures have no position info beyond the `near "…"` context SQLite puts in
/// the message itself — unlike Postgres, which has a real error cursor to
/// enrich with.
fn query_error(err: sqlx::Error) -> DbError {
    DbError::Query(err.to_string())
}

/// Why SQL text holding a NUL character never reaches SQLite — see
/// [`SqlText`](door::SqlText).
pub const NUL_IN_SQL: &str =
    "This statement contains a NUL character, which can't be sent to SQLite.";

use door::SqlText;

/// The affected-row count to report for `sql`, given the connection's
/// `total_changes()` before and after it and the count the driver reported.
///
/// The driver reports `sqlite3_changes()`, which only an `INSERT`, `UPDATE` or
/// `DELETE` sets (`REPLACE` is an `INSERT`; any of them may be led by `WITH`):
/// any other statement leaves it at the last such statement's count. Measured
/// after a two-row `INSERT`: `BEGIN`, `ROLLBACK`, `SAVEPOINT`, `CREATE TABLE`,
/// `CREATE INDEX`, `DROP TABLE`, `ANALYZE` and `VACUUM` each reported 2. So
/// the count is the driver's only for a statement led by one of those words,
/// and 0 for every other.
///
/// The keyword alone is not enough, because a `WITH` can lead a plain
/// `SELECT`, which leaves the count stale too. `total_changes()` counts every
/// row an `INSERT`, `UPDATE` or `DELETE` changed on the connection, so when
/// it did not move, the statement changed no row and the count is 0.
///
/// The counter alone is not enough either. A virtual table's module writes
/// its shadow tables with ordinary DML, which moves the counter and sets the
/// driver's count, under a statement that is not a write. Measured on the
/// bundled SQLite 3.46: `CREATE VIRTUAL TABLE … USING fts5(x)` moved it by 3
/// and `… USING rtree(…)` by 1, `ALTER TABLE … RENAME` of an FTS5 table by 4
/// and `DROP TABLE` of one holding a row by 4, and each left the driver's
/// count at 1.
///
/// The counter's difference is not reported in place of the driver's count:
/// it also counts the rows a trigger changed, which `sqlite3_changes()`, like
/// Postgres's command tag, leaves out.
fn affected_rows(sql: &str, total_before: i64, total_after: i64, reported: u64) -> u64 {
    let writes = matches!(
        first_keyword(sql, Dialect::Sqlite)
            .to_ascii_lowercase()
            .as_str(),
        "insert" | "update" | "delete" | "replace" | "with"
    );
    if writes && total_after != total_before {
        reported
    } else {
        0
    }
}

/// The one door SQL text passes through to sqlx-sqlite — see [`SqlText`].
mod door {
    use super::{query_error, sqlx_common, DbError, QueryResult, SqliteQuery, NUL_IN_SQL};

    /// SQL text this backend may hand to sqlx-sqlite: text holding no NUL.
    ///
    /// **The one door.** Every statement the SQLite backend runs, describes or
    /// prepares goes through a value of this type. It lives in a module of
    /// its own so the compiler holds every construction to [`SqlText::new`],
    /// and every call that hands text to the driver lives in that module —
    /// `the_driver_is_handed_text_only_through_sql_text` holds the rest of
    /// this file to that, and `only_known_files_hand_sql_text_to_sqlx` every
    /// other file. A choke point rather than a proof per path, because the
    /// paths are many (free-form queries, page reads, edits, imports,
    /// exports, introspection) and the next one would need a proof of its own.
    ///
    /// The hang it prevents, in sqlx-sqlite 0.8.6 and unchanged in 0.9.0: sqlx
    /// splits a query into statements by calling `sqlite3_prepare_v3` in a loop,
    /// advancing by the tail SQLite reports. SQLite reads only up to a NUL, so
    /// once the statements before one are compiled the tail stays on the NUL —
    /// nothing consumed, and the loop never ends. The connection's worker thread
    /// spins a core for good, out of reach of any timeout. Measured with `SELECT
    /// 1` followed by a NUL. A NUL *inside* a quoted name or string literal fails
    /// cleanly instead, since SQLite reads the token as unterminated, and hubro's
    /// own builders quote every name: a table whose `sqlite_master` name holds a
    /// NUL — which a crafted file can have, and SQLite opens — gets
    /// `unrecognized token` rather than a hang. That holds only while every
    /// builder keeps quoting, which is the per-path proof this type replaces, and
    /// refusing every NUL gives that table a sentence instead.
    #[derive(Debug, Clone, Copy)]
    pub(in crate::db) struct SqlText<'a>(&'a str);

    impl<'a> SqlText<'a> {
        /// `sql`, or [`NUL_IN_SQL`] when it holds a NUL anywhere. The only
        /// way to build one.
        pub(super) fn new(sql: &'a str) -> Result<Self, DbError> {
            if sql.contains('\0') {
                return Err(DbError::Query(NUL_IN_SQL.to_string()));
            }
            Ok(SqlText(sql))
        }

        /// A prepared (not simple-protocol) query over this text.
        pub(super) fn query(self) -> SqliteQuery<'a> {
            sqlx::query(self.0)
        }

        /// The checked text, for the generic paths' describes and error
        /// messages ([`SqlxBackend::sql`](super::sqlx_common::SqlxBackend::sql)).
        pub(super) fn as_str(self) -> &'a str {
            self.0
        }

        /// Runs this text without decoding rows — see [`sqlx_common::execute`] —
        /// and returns its affected-row count, which is the driver's only when
        /// the text changed a row: see [`affected_rows`](super::affected_rows).
        ///
        /// Takes a pool or a connection, and runs the text and both reads of
        /// `total_changes()` on one connection, since the counter is per
        /// connection.
        pub(super) async fn execute<'c, A>(self, acquire: A) -> Result<u64, DbError>
        where
            A: sqlx::Acquire<'c, Database = sqlx::Sqlite>,
        {
            let mut conn = acquire.acquire().await.map_err(query_error)?;
            let before = total_changes(&mut conn).await?;
            let reported = sqlx_common::execute(&mut *conn, self.0, query_error).await?;
            let after = total_changes(&mut conn).await?;
            Ok(super::affected_rows(self.0, before, after, reported))
        }

        /// See [`sqlx_common::fill_headers`].
        pub(super) async fn fill_headers<'c, E>(self, result: &mut QueryResult, executor: E)
        where
            E: sqlx::Executor<'c, Database = sqlx::Sqlite>,
        {
            sqlx_common::fill_headers(result, executor, self.0).await;
        }
    }

    /// The connection's `total_changes()`: every row an `INSERT`, `UPDATE` or
    /// `DELETE` has changed on it since it opened, triggers' included.
    async fn total_changes(conn: &mut sqlx::SqliteConnection) -> Result<i64, DbError> {
        sqlx::query_scalar("SELECT total_changes()")
            .fetch_one(conn)
            .await
            .map_err(query_error)
    }
}

/// Executes a statement without decoding rows, returning its affected-row
/// count (0 for a statement that changed no row — see [`affected_rows`]).
pub async fn execute(pool: &SqlitePool, sql: &str) -> Result<u64, DbError> {
    SqlText::new(sql)?.execute(pool).await
}

/// Executes parameterized writes inside ONE transaction, committing only
/// when every statement affected exactly its `expected_rows` rows — see
/// [`sqlx_common::execute_all_checked`]. A statement holding a NUL is refused
/// before the transaction opens.
pub async fn execute_all_checked(
    pool: &SqlitePool,
    statements: &[CheckedStatement],
) -> Result<(), (Option<usize>, DbError)> {
    sqlx_common::execute_all_checked::<SqliteBackend>(pool, statements).await
}

type SqliteQuery<'q> = sqlx::query::Query<'q, sqlx::Sqlite, sqlx::sqlite::SqliteArguments<'q>>;

/// Like [`query`], with bound parameters (used by the paged table reader so
/// filter values never touch the SQL text) — see
/// [`sqlx_common::query_with`], including why it recovers no headers.
pub async fn query_with(
    pool: &SqlitePool,
    sql: &str,
    params: &[Value],
) -> Result<QueryResult, DbError> {
    sqlx_common::query_with::<SqliteBackend, _>(pool, SqlText::new(sql)?, params).await
}

/// A free-form query with its rows and cells capped — see
/// [`sqlx_common::query_capped`].
pub async fn query_capped(
    pool: &SqlitePool,
    sql: &str,
    params: &[Value],
    max_rows: u64,
    cell_cap: usize,
) -> Result<(QueryResult, bool), DbError> {
    let sql = SqlText::new(sql)?;
    sqlx_common::query_capped::<SqliteBackend>(pool, sql, params, max_rows, cell_cap).await
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
    let sql = SqlText::new(sql)?;
    let (mut result, truncated) = sqlx_common::query_capped_rows::<SqliteBackend, _>(
        &mut *conn,
        sql,
        &[],
        max_rows,
        cell_cap,
    )
    .await?;
    sql.fill_headers(&mut result, &mut *conn).await;
    Ok((result, truncated))
}

/// Runs a non-row statement on a single connection (e.g. one borrowed from a
/// transaction) rather than the pool, returning affected rows — the write
/// path for statements inside an atomically-wrapped script.
pub async fn execute_conn(
    conn: &mut sqlx::sqlite::SqliteConnection,
    sql: &str,
) -> Result<u64, DbError> {
    SqlText::new(sql)?.execute(conn).await
}

/// Ends a script's hold on its connection
/// ([`ScriptConn`](super::registry::ScriptConn)): rolls back any transaction
/// the script left open, and reports whether there was one.
///
/// SQLite says whether a transaction is open only through
/// `sqlite3_get_autocommit`, which sqlx keeps to itself, so the probe is a
/// `BEGIN`: SQLite refuses one inside a transaction. Either way a `ROLLBACK`
/// follows — of the script's transaction when the `BEGIN` was refused, of the
/// probe's own (which took no lock: a deferred `BEGIN` reads nothing) when it
/// was not. See [`script_end`] for what the two outcomes mean together.
pub async fn end_script_conn(conn: &mut sqlx::sqlite::SqliteConnection) -> Result<bool, DbError> {
    let probe = execute_conn(&mut *conn, "BEGIN").await;
    let rollback = execute_conn(&mut *conn, "ROLLBACK").await;
    script_end(probe.is_ok(), rollback)
}

/// What [`end_script_conn`]'s probe and rollback mean together, over plain
/// outcomes so a test can execute every combination.
///
/// - `BEGIN` refused, `ROLLBACK` taken: the script had left a transaction
///   open, and it is now rolled back — `true`.
/// - `BEGIN` taken, `ROLLBACK` taken: nothing was open, and the probe's own
///   transaction is closed again — `false`.
/// - `ROLLBACK` refused, whatever the `BEGIN` did: the connection may still be
///   inside a transaction — the probe's, or one nothing could end — so the
///   error is returned: hubro cannot say what was open when the connection
///   closed.
pub(super) fn script_end(begun: bool, rollback: Result<u64, DbError>) -> Result<bool, DbError> {
    rollback.map(|_| !begun)
}

/// [`execute_conn`] with bound parameters — see
/// [`sqlx_common::execute_with`]. The driver's count, unlike
/// [`execute_conn`]'s: an import runs only the `INSERT`s it builds.
pub async fn execute_with_conn(
    conn: &mut sqlx::sqlite::SqliteConnection,
    sql: &str,
    params: &[Value],
) -> Result<u64, DbError> {
    sqlx_common::execute_with::<SqliteBackend, _>(conn, SqlText::new(sql)?, params).await
}

/// Streams a query to `out` in the given format — see
/// [`sqlx_common::export_query`]. The export holds one of the pool's
/// connections for its read and, for an empty result, the describe.
pub async fn export(
    pool: &SqlitePool,
    sql: &str,
    params: &[Value],
    format: ExportFormat,
    out: &mut impl Write,
) -> Result<u64, DbError> {
    let sql = SqlText::new(sql)?;
    let mut conn = pool.acquire().await.map_err(query_error)?;
    sqlx_common::export_query::<SqliteBackend>(&mut conn, sql, params, format, out).await
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
    let rows = SqlText::new(&sql)?
        .query()
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
    let (mut indexes, pk_indexed) = table_indexes(pool).await?;
    let mut foreign_keys = table_foreign_keys(pool).await?;

    let mut tables = Vec::with_capacity(entries.len());
    for entry in entries {
        let restriction = restriction_for(&entry, !unreadable.contains(&entry.name));
        let mut table_columns = columns.remove(&entry.name).unwrap_or_default();
        let ordinary_table = !entry.view && !entry.virtual_table;
        if let Some(alias) = rowid_alias(
            ordinary_table,
            &table_columns,
            pk_indexed.contains(&entry.name),
        ) {
            let column = &mut table_columns[alias];
            column.generated = Generated::RowidAlias;
            // Whatever the declaration says: `UPDATE … SET id = NULL` is a
            // datatype mismatch, so offering NULL in the editor would only
            // offer a failure.
            column.nullable = false;
        }
        let MasterEntry { name, view, .. } = entry;
        tables.push(TableMeta {
            schema: None,
            columns: table_columns,
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
    canonicalize_foreign_keys(&mut tables);
    Ok(tables)
}

/// Rewrites every foreign key's referenced table and columns to the names
/// they resolve to, as `sqlite_master` and the table's columns spell them.
///
/// `pragma_foreign_key_list` reports a reference *as written* in the
/// `REFERENCES` clause, and SQLite resolves those names case-insensitively
/// (ASCII only): `REFERENCES Parent(ID)` is a working key into `parent(id)`.
/// Left as written, the jump looked for a table named `Parent` in the loaded
/// schema, found none, and the key's link led nowhere. A name that resolves
/// to nothing is left as it is — that reference really is dangling.
fn canonicalize_foreign_keys(tables: &mut [TableMeta]) {
    let columns_of: HashMap<String, (String, Vec<String>)> = tables
        .iter()
        .map(|t| {
            (
                t.name.to_ascii_lowercase(),
                (
                    t.name.clone(),
                    t.columns.iter().map(|c| c.name.clone()).collect(),
                ),
            )
        })
        .collect();
    for table in tables.iter_mut() {
        for fk in &mut table.foreign_keys {
            let Some((name, columns)) = columns_of.get(&fk.referenced_table.to_ascii_lowercase())
            else {
                continue;
            };
            fk.referenced_table = name.clone();
            for referenced in fk.referenced_columns.iter_mut().flatten() {
                if let Some(real) = columns.iter().find(|c| c.eq_ignore_ascii_case(referenced)) {
                    *referenced = real.clone();
                }
            }
        }
    }
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
         FROM sqlite_master m JOIN pragma_table_xinfo(m.name, 'main') x \
         WHERE {MASTER_FILTER} AND {ORDINARY_TABLE} \
         ORDER BY m.name, x.cid"
    );
    let rows = SqlText::new(&sql)?
        .query()
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
    let sql =
        format!("SELECT {XINFO_COLUMNS} FROM pragma_table_xinfo(?1, 'main') x ORDER BY x.cid");
    let sql = SqlText::new(&sql)?;
    let (resolved, unreadable) = read_each(&pending, |name| async move {
        let rows = sql
            .query()
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

/// Which of a table's columns is its rowid alias, if any — the column SQLite
/// fills with the next rowid when an insert leaves it out or sends NULL.
///
/// Decided on a catalog fact rather than on the declared type: SQLite keeps
/// a separate `origin = 'pk'` index behind every primary key *except* the
/// rowid alias, which is the key of the table's own b-tree. That one fact
/// sorts every shape the way SQLite behaves, which the type does not —
/// measured on SQLite 3.46 (`tests/db_sqlite.rs` measures it again):
///
/// - the alias, no pk index: `id INTEGER PRIMARY KEY` in any case, with a
///   quoted type, `AUTOINCREMENT` or `STRICT`; and the table constraints
///   `PRIMARY KEY(id)` and `PRIMARY KEY(id DESC)`;
/// - not the alias, a pk index: the column constraint `INTEGER PRIMARY KEY
///   DESC` (SQLite's documented quirk), `INT`, `BIGINT`, `INTEGER(4)` and
///   `TEXT` keys, any key of a `WITHOUT ROWID` table (where the old guess
///   from the declared type went wrong), and every composite key.
///
/// `ordinary_table` is defence in depth, not what decides today: a view or a
/// virtual table has no index list, so the absent pk index would prove
/// nothing about it, but `pragma_table_xinfo` also reports no key column for
/// either (measured for views and FTS5, and pinned by
/// `views_virtual_and_generated_shapes_never_claim_the_alias` in
/// `tests/db_sqlite.rs`), so the key-column count already rules both out. The
/// guard keeps that true for a virtual-table module that does declare a key.
pub(super) fn rowid_alias(
    ordinary_table: bool,
    columns: &[ColumnMeta],
    has_pk_index: bool,
) -> Option<usize> {
    if !ordinary_table || has_pk_index {
        return None;
    }
    let mut key = columns
        .iter()
        .enumerate()
        .filter(|(_, column)| column.primary_key_position.is_some());
    match (key.next(), key.next()) {
        (Some((index, _)), None) => Some(index),
        _ => None,
    }
}

/// The constraint behind an index, from `pragma_index_list.origin`: `pk` for
/// a PRIMARY KEY's, `u` for a UNIQUE constraint's, `c` for one made by
/// `CREATE INDEX`. Measured on SQLite 3.46: `DROP INDEX` refuses both
/// constraint kinds ("index associated with UNIQUE or PRIMARY KEY constraint
/// cannot be dropped") and drops a `c` index, unique or not.
pub(super) fn index_constraint(origin: &str) -> Option<IndexConstraint> {
    match origin {
        "pk" => Some(IndexConstraint::PrimaryKey),
        "u" => Some(IndexConstraint::Unique),
        _ => None,
    }
}

/// Indexes of every table, keyed by table name, and the names of the tables
/// that have an `origin = 'pk'` index (see [`rowid_alias`]). Two queries: the
/// per-table index list, and every index's key columns keyed by index name —
/// index names are unique across a SQLite schema, so one flat map covers all
/// tables.
async fn table_indexes(
    pool: &SqlitePool,
) -> Result<(HashMap<String, Vec<IndexMeta>>, HashSet<String>), DbError> {
    let sql = format!(
        "SELECT il.name AS index_name, ii.seqno AS seqno, ii.name AS column_name \
         FROM sqlite_master m \
         JOIN pragma_index_list(m.name, 'main') il \
         JOIN pragma_index_info(il.name, 'main') ii \
         WHERE {MASTER_FILTER}"
    );
    let column_rows = SqlText::new(&sql)?
        .query()
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
                il.\"unique\" AS \"unique\", il.partial AS partial, \
                il.origin AS origin \
         FROM sqlite_master m JOIN pragma_index_list(m.name, 'main') il \
         WHERE {MASTER_FILTER} \
         ORDER BY m.name, il.seq"
    );
    let rows = SqlText::new(&sql)?
        .query()
        .fetch_all(pool)
        .await
        .map_err(|e| DbError::Introspect(e.to_string()))?;
    let mut indexes: HashMap<String, Vec<IndexMeta>> = HashMap::new();
    let mut pk_indexed = HashSet::new();
    for row in rows {
        let table: String = get(&row, "table_name")?;
        let name: String = get(&row, "index_name")?;
        let unique: i64 = get(&row, "unique")?;
        let partial: i64 = get(&row, "partial")?;
        let origin: String = get(&row, "origin")?;
        let constraint = index_constraint(&origin);
        if constraint == Some(IndexConstraint::PrimaryKey) {
            pk_indexed.insert(table.clone());
        }
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
            constraint,
        });
    }
    Ok((indexes, pk_indexed))
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
         FROM sqlite_master m JOIN pragma_foreign_key_list(m.name, 'main') fk \
         WHERE {MASTER_FILTER}"
    );
    let rows = SqlText::new(&sql)?
        .query()
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
    let analyzed = SqlText::new(
        "SELECT count(*) AS n FROM sqlite_master WHERE type = 'table' AND name = 'sqlite_stat1'",
    )?
    .query()
    .fetch_one(pool)
    .await
    .map_err(|e| DbError::Introspect(e.to_string()))?;
    let analyzed: i64 = get(&analyzed, "n")?;
    if analyzed == 0 {
        return Ok(TableStats::default());
    }
    // Any of the table's rows will do: ANALYZE writes one per index, plus one
    // with a NULL `idx` when there is no index, and every one of them opens
    // with the table's own row count.
    let stat = SqlText::new("SELECT stat FROM sqlite_stat1 WHERE tbl = ?1 AND stat IS NOT NULL")?
        .query()
        .bind(&table.name)
        .fetch_optional(pool)
        .await
        .map_err(|e| DbError::Introspect(e.to_string()))?;
    let stat: Option<String> = stat.map(|row| get(&row, "stat")).transpose()?;
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
        SqlText::new("SELECT type, sql FROM sqlite_master WHERE name = ?1 AND type <> 'trigger'")?
            .query()
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
    let rows = SqlText::new(
        "SELECT sql FROM sqlite_master \
         WHERE type = 'index' AND tbl_name = ?1 AND sql IS NOT NULL \
         ORDER BY name",
    )?
    .query()
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
    fn a_statement_that_changed_no_row_reports_none_whatever_the_driver_says() {
        // `BEGIN` after a two-row INSERT: the driver repeats the 2.
        assert_eq!(affected_rows("BEGIN", 7, 7, 2), 0);
        // A write: the driver's count, not the counter's difference, which
        // also holds the rows its triggers changed.
        assert_eq!(affected_rows("INSERT INTO t VALUES (1)", 7, 10, 1), 1);
        assert_eq!(
            affected_rows("-- c\n replace INTO t VALUES (1)", 7, 9, 2),
            2
        );
        assert_eq!(affected_rows("UPDATE t SET a = 1", 7, 8, 1), 1);
        assert_eq!(affected_rows("DELETE FROM t", 7, 8, 1), 1);
        assert_eq!(
            affected_rows("WITH x AS (SELECT 1) DELETE FROM t", 7, 8, 1),
            1
        );
        // A WITH-led read leaves the count stale and the counter still.
        assert_eq!(
            affected_rows("WITH x AS (SELECT 1) SELECT * FROM x", 7, 7, 2),
            0
        );
        // A virtual table's module moves the counter and sets the driver's
        // count under DDL: 3.46 reports 1 for each of these.
        assert_eq!(
            affected_rows("CREATE VIRTUAL TABLE f USING fts5(x)", 0, 3, 1),
            0
        );
        assert_eq!(affected_rows("ALTER TABLE f RENAME TO g", 3, 7, 1), 0);
        assert_eq!(affected_rows("DROP TABLE g", 7, 11, 1), 0);
    }

    /// The probe's two outcomes, and what [`end_script_conn`] makes of them.
    /// Only a refused `BEGIN` says a transaction was open; a refused
    /// `ROLLBACK` leaves the connection's state unknown, whatever came first.
    #[test]
    fn a_refused_begin_is_the_one_sign_of_an_open_transaction() {
        let refused = || Err(DbError::Query("refused".into()));
        assert_eq!(script_end(false, Ok(0)), Ok(true));
        assert_eq!(script_end(true, Ok(0)), Ok(false));
        assert!(script_end(true, refused()).is_err());
        assert!(script_end(false, refused()).is_err());
    }

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

    #[test]
    fn sql_text_refuses_a_nul_wherever_it_sits() {
        for sql in [
            "SELECT 1\0",
            "\0SELECT 1",
            "SELECT 1; \0 DELETE FROM t",
            "SELECT 1 -- \0",
            "SELECT * FROM \"t\0x\"",
            "SELECT '\0'",
            "\0",
        ] {
            assert_eq!(
                SqlText::new(sql).err(),
                Some(DbError::Query(NUL_IN_SQL.to_string())),
                "{sql:?}"
            );
        }
        for sql in ["SELECT 1", "", "SELECT '\\0'", "SELECT char(0)"] {
            assert!(SqlText::new(sql).is_ok(), "{sql:?}");
        }
    }

    /// A batch with a NUL in any statement is refused before its transaction
    /// opens: the statements ahead of it never run, rather than running and
    /// rolling back. The first statement is set up to fail its row count, so
    /// running it at all would report that failure, at index 0, instead.
    #[tokio::test]
    async fn a_checked_batch_holding_a_nul_runs_none_of_its_statements() {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        SqlText::new("CREATE TABLE t (a INTEGER); INSERT INTO t VALUES (1)")
            .unwrap()
            .execute(&pool)
            .await
            .unwrap();
        let statements = [
            CheckedStatement {
                sql: "DELETE FROM t".into(),
                params: vec![],
                expected_rows: 2,
            },
            CheckedStatement {
                sql: "DELETE FROM t;\0".into(),
                params: vec![],
                expected_rows: 0,
            },
        ];
        let outcome = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            execute_all_checked(&pool, &statements),
        )
        .await
        .expect("a NUL must be refused, not handed to the driver");
        assert_eq!(
            outcome,
            Err((Some(1), DbError::Query(NUL_IN_SQL.to_string())))
        );
        let rows = query(&pool, "SELECT count(*) FROM t").await.unwrap();
        assert_eq!(rows.rows, vec![vec![Value::Integer(1)]]);
    }

    /// `source` outside its test modules, line by line, with comments removed
    /// ([`crate::source_scan::code_outside_test_modules`]) and the lines that
    /// leaves empty dropped.
    fn shipped_code(source: &str) -> Vec<String> {
        crate::source_scan::code_outside_test_modules(source)
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(str::to_string)
            .collect()
    }

    fn is_ident(c: char) -> bool {
        c.is_alphanumeric() || c == '_'
    }

    /// Each occurrence of `word` in `code` as a whole identifier, by offset.
    fn ident_at<'a>(code: &'a str, word: &'a str) -> impl Iterator<Item = usize> + 'a {
        code.match_indices(word)
            .map(|(at, _)| at)
            .filter(move |&at| {
                !code[..at].chars().next_back().is_some_and(is_ident)
                    && !code[at + word.len()..].chars().next().is_some_and(is_ident)
            })
    }

    /// What may follow `sqlx::` outside the door: types, traits and modules
    /// none of which takes SQL text. An allowlist, so a text entry point
    /// nobody listed — `query`, `raw_sql`, `Executor`, the `prelude`, a macro
    /// — is refused by default. `query` is here only as the module in
    /// `sqlx::query::Query`, the type the door hands out.
    const SQLX_ALLOWED: &[&str] = &[
        "sqlite",
        "Sqlite",
        "SqlitePool",
        "SqlitePoolOptions",
        "SqliteConnection",
        "SqliteConnectOptions",
        "SqliteRow",
        "SqliteArguments",
        "Postgres",
        "pool",
        "PoolConnection",
        "Transaction",
        "Connection",
        "Row",
        "TypeInfo",
        "ValueRef",
        "Error",
    ];

    /// What may follow `sqlx_common::` outside the door: the helpers that
    /// take rows, a pool or a transaction, not text, the backend trait, and
    /// the generic paths ([`COMMON_GENERIC`], [`COMMON_PREPARING`]) — see
    /// [`common_allowed`], which is the list the detector reads.
    const COMMON_ALLOWED: &[&str] = &[
        "get",
        "acquire_for_run",
        "begin_for_run",
        "collect_all",
        "collect_capped",
        "export_stream",
        "export_empty",
        "commit_tx",
        "rollback_tx",
        "SqlxBackend",
    ];

    /// The generic sqlx paths this file calls with text. They do take text,
    /// which is why each is listed by name rather than the allowlist taking
    /// any: on SQLite their text is a [`SqlText`], so the door has already
    /// checked it — `the_generic_paths_take_text_only_as_backend_text` pins
    /// that each takes it so and that SQLite's `Text` is `SqlText`, and
    /// `every_sqlite_entry_point_refuses_a_nul` runs them.
    const COMMON_GENERIC: &[&str] = &[
        "query_all",
        "query_with",
        "query_capped",
        "query_capped_rows",
        "execute_with",
        "export_query",
    ];

    /// The generic paths that take whole statements rather than a `Text`, so
    /// prepare each themselves, through
    /// [`SqlxBackend::prepare`](sqlx_common::SqlxBackend).
    const COMMON_PREPARING: &[&str] = &["execute_all_checked"];

    /// Whether `name` may follow `sqlx_common::` outside the door: the three
    /// lists above, so none of them can drift from what the detector allows.
    fn common_allowed(name: &str) -> bool {
        [COMMON_ALLOWED, COMMON_GENERIC, COMMON_PREPARING]
            .iter()
            .any(|list| list.contains(&name))
    }

    /// Every way `code` reaches the driver with text, as `(offset, what)`.
    ///
    /// - A path through `sqlx::` or `sqlx_common::` naming anything off the
    ///   allowlists, down to its first type or trait, a macro
    ///   (`sqlx::query!`) included. `sqlx::Executor::execute` fails on
    ///   `Executor`.
    /// - A `use` of either — `pub` or not, anywhere in the file — that
    ///   imports anything off them, renames anything (`use sqlx as driver`),
    ///   or globs (`use sqlx::prelude::*`).
    /// - A macro metavariable straight after either (`sqlx::$m`).
    /// - `extern crate sqlx as …`, and the `Executor` trait by any route:
    ///   it is what lets a pool or connection run a bare `&str`.
    fn reaches_driver(code: &str) -> Vec<(usize, String)> {
        let mut found = Vec::new();
        let sqlx_allowed: fn(&str) -> bool = |name| SQLX_ALLOWED.contains(&name);
        let common: fn(&str) -> bool = common_allowed;
        for (root, allowed) in [("sqlx", sqlx_allowed), ("sqlx_common", common)] {
            for at in ident_at(code, root) {
                let rest = &code[at + root.len()..];
                let Some(rest) = rest.strip_prefix("::") else {
                    continue;
                };
                let path: String = rest
                    .chars()
                    .take_while(|c| is_ident(*c) || *c == ':')
                    .collect();
                let path = path.trim_end_matches(':');
                let macro_call = rest[path.len()..].starts_with('!');
                // Modules down to the first type or trait, which is checked
                // too; what follows one (`Error::Database`) is its own item.
                let mut checked = Vec::new();
                for segment in path.split("::") {
                    checked.push(segment);
                    if segment.starts_with(char::is_uppercase) {
                        break;
                    }
                }
                let named = checked.iter().all(|segment| allowed(segment))
                    || (root == "sqlx" && checked == ["query", "Query"]);
                // A macro metavariable (`sqlx::$m($s)`) names whatever its
                // caller passes, so no allowlist can vouch for it.
                if rest.starts_with('$') {
                    found.push((at, format!("{root}::$ in a macro")));
                } else if !path.is_empty() && (macro_call || !named) {
                    found.push((at, format!("{root}::{path}")));
                }
            }
        }
        for at in ident_at(code, "use") {
            let Some(end) = code[at..].find(';') else {
                continue;
            };
            let statement = &code[at..at + end];
            let mut tokens = statement
                .split(|c: char| !is_ident(c))
                .filter(|t| !t.is_empty());
            if !tokens.any(|t| t == "sqlx" || t == "sqlx_common") {
                continue;
            }
            if statement.contains('*') {
                found.push((at, format!("glob: {statement}")));
            }
            for token in tokens {
                let fine = SQLX_ALLOWED.contains(&token)
                    || common_allowed(token)
                    || ["sqlx", "sqlx_common", "self", "as", "_"].contains(&token);
                if !fine {
                    found.push((at, format!("`{token}` in: {statement}")));
                }
            }
        }
        for root in ["sqlx", "sqlx_common"] {
            for at in ident_at(code, root) {
                if code[at + root.len()..].starts_with(" as ") {
                    found.push((at, format!("{root} renamed")));
                }
            }
        }
        for at in ident_at(code, "Executor") {
            found.push((at, "Executor".to_string()));
        }
        found
    }

    /// This file's shipped code with `mod door` cut out.
    fn sqlite_outside_the_door() -> String {
        let lines = shipped_code(include_str!("sqlite.rs"));
        let open = lines
            .iter()
            .position(|line| *line == "mod door {")
            .expect("the door is in this file");
        let close = open
            + lines[open..]
                .iter()
                .position(|line| *line == "}")
                .expect("the door's module ends");
        [&lines[..open], &lines[close + 1..]].concat().join("\n")
    }

    /// The other half of [`SqlText`]'s guarantee: outside `mod door`, this
    /// file reaches the driver with text by no route [`reaches_driver`]
    /// knows. The compiler already confines construction to
    /// [`SqlText::new`]; this confines everything else. Mutated by calling
    /// `sqlx::query` directly in one function, importing `fill_headers` by
    /// its bare name, calling `sqlx_common::execute` from `execute_conn`, and
    /// `use sqlx as driver` with `driver::query`: each fails here.
    #[test]
    fn the_driver_is_handed_text_only_through_sql_text() {
        let code = sqlite_outside_the_door();
        let found = reaches_driver(&code);
        assert!(
            found.is_empty(),
            "outside `mod door`, sqlite.rs hands text to the driver unchecked: {found:?}"
        );
    }

    /// The detector itself, against each route it exists to see — so a
    /// loosened rule fails here rather than reading as a clean file.
    #[test]
    fn the_detector_sees_every_route_to_the_driver() {
        for code in [
            "sqlx::query(sql)",
            "sqlx::query!(\"SELECT 1\")",
            "sqlx::raw_sql(sql)",
            "sqlx::query_scalar(sql)",
            "::sqlx::query(sql)",
            "sqlx_common::execute(conn, sql, e)",
            "super::sqlx_common::fill_headers(r, pool, sql)",
            "use sqlx::query;",
            "pub(super) use sqlx::{query};",
            "pub use sqlx::Executor;",
            "use sqlx::{Row as _, Executor as _};",
            "use sqlx::prelude::*;",
            "use sqlx::*;",
            "use sqlx as driver;",
            "use super::sqlx_common as c;",
            "use super::sqlx_common::{self as c};",
            "use super::sqlx_common::{self, fill_headers};",
            "extern crate sqlx as driver;",
            "fn f<E: Executor<'static>>() {}",
            "sqlx::Executor::execute(pool, sql)",
            "macro_rules! via { ($m:ident, $s:expr) => { sqlx::$m($s) } }",
            "macro_rules! via { ($f:ident, $s:expr) => { sqlx_common::$f(c, $s, e) } }",
        ] {
            assert!(!reaches_driver(code).is_empty(), "missed: {code}");
        }
        for code in [
            "use sqlx::sqlite::{SqlitePool, SqliteRow};",
            "use sqlx::{Connection as _, Row as _};",
            "use super::sqlx_common::{self, get};",
            "type Q<'q> = sqlx::query::Query<'q, sqlx::Sqlite, sqlx::sqlite::SqliteArguments<'q>>;",
            "sqlx_common::collect_all(stream, decode, err)",
            "sqlx::Connection::close(conn)",
            "Err(sqlx::Error::Database(e)) => {}",
            "let used = 1;",
        ] {
            assert_eq!(reaches_driver(code), vec![], "{code}");
        }
    }

    /// What makes [`COMMON_GENERIC`] and [`COMMON_PREPARING`] safe to allow,
    /// as properties rather than spellings:
    ///
    /// - each path in [`COMMON_GENERIC`] takes exactly one text parameter,
    ///   `text: B::Text<'q>`, and every other parameter is one of a closed
    ///   list of kinds that carry no text — so a `&str`, `String` or bytes
    ///   parameter added to one fails here, whatever its name;
    /// - each path in [`COMMON_PREPARING`] prepares its statements through
    ///   the backend before anything else;
    /// - across every shipped file there are exactly two `SqlxBackend`
    ///   impls, one per driver, and the SQLite one's `Text` is [`SqlText`] —
    ///   so no second impl over `sqlx::Sqlite`, in a file the driver scan
    ///   exempts (`postgres.rs`) or anywhere else, can hand the generic paths
    ///   SQLite text the door never saw.
    #[test]
    fn the_generic_paths_take_text_only_as_backend_text() {
        use crate::source_scan::{block_end, code_outside_test_modules, rust_files};
        let collapsed = |code: &str| code.split_whitespace().collect::<Vec<_>>().join(" ");
        let common = collapsed(&code_outside_test_modules(include_str!("sqlx_common.rs")));
        let at = |name: &str| {
            common
                .find(&format!("pub(super) async fn {name}<"))
                .unwrap_or_else(|| panic!("no generic fn {name}"))
        };
        const TEXTLESS: &[&str] = &[
            "executor: E",
            "pool: &sqlx::Pool<B::DB>",
            "conn: &mut <B::DB as Database>::Connection",
            "params: &[Value]",
            "max_rows: u64",
            "cell_cap: usize",
            "format: ExportFormat",
            "out: &mut impl Write",
        ];
        for name in COMMON_GENERIC {
            let start = at(name);
            let sig = start + "pub(super) async fn ".len();
            let open = sig + common[sig..].find('(').unwrap();
            let close = open + common[open..].find(')').unwrap();
            let params: Vec<&str> = common[open + 1..close]
                .split(',')
                .map(str::trim)
                .filter(|p| !p.is_empty())
                .collect();
            let texts = params.iter().filter(|p| **p == "text: B::Text<'q>").count();
            assert_eq!(
                texts, 1,
                "{name} must take one `text: B::Text<'q>`: {params:?}"
            );
            for param in &params {
                assert!(
                    *param == "text: B::Text<'q>" || TEXTLESS.contains(param),
                    "{name} takes `{param}`, which is not a kind known to carry no text"
                );
            }
        }
        for name in COMMON_PREPARING {
            let start = at(name);
            let body = &common[start..start + block_end(&common[start..]).unwrap()];
            let body = &body[body.find('{').unwrap()..];
            assert!(
                body.starts_with("{ let texts = statements .iter() .enumerate() .map(|(index, statement)| B::prepare(&statement.sql)"),
                "{name} does not prepare its statements through the backend first"
            );
        }
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut impls = Vec::new();
        for file in rust_files(&root) {
            let code = collapsed(&code_outside_test_modules(
                &std::fs::read_to_string(&file).unwrap(),
            ));
            for (at, _) in code.match_indices("SqlxBackend for ") {
                let start = code[..at].rfind("impl").expect("an impl header");
                let end = start + block_end(&code[start..]).expect("the impl closes");
                impls.push((file.clone(), code[start..end].to_string()));
            }
        }
        assert_eq!(
            impls.len(),
            2,
            "one SqlxBackend impl per driver: {impls:#?}"
        );
        let sqlite: Vec<_> = impls
            .iter()
            .filter(|(_, block)| !block.contains("type DB = sqlx::Postgres;"))
            .collect();
        assert_eq!(
            sqlite.len(),
            1,
            "one impl that is not Postgres's: {impls:#?}"
        );
        let (file, block) = sqlite[0];
        assert!(file.ends_with("db/sqlite.rs"), "{file:?}");
        assert!(block.contains("type DB = sqlx::Sqlite;"), "{block}");
        assert!(
            block.contains("type Text<'q> = SqlText<'q>;"),
            "SQLite's generic text is not SqlText: {block}"
        );
    }

    /// Every SQLite entry point that takes SQL text refuses one holding a
    /// NUL with [`NUL_IN_SQL`], the generic paths' callers included. The NUL
    /// sits inside a string literal, where SQLite fails on it cleanly
    /// (`unrecognized token`) rather than hanging, so a path that bypassed
    /// the door fails here with that error instead of spinning a core.
    #[tokio::test]
    async fn every_sqlite_entry_point_refuses_a_nul() {
        let pool = one_connection().await;
        run(&pool, "CREATE TABLE t (a TEXT)").await;
        let read = "SELECT 'a\0'";
        let write = "INSERT INTO t VALUES ('a\0')";
        let refused = || DbError::Query(NUL_IN_SQL.to_string());
        fn within<T>(what: &str) -> impl FnOnce(Result<T, tokio::time::error::Elapsed>) -> T + '_ {
            move |outcome| outcome.unwrap_or_else(|_| panic!("{what} handed a NUL to the driver"))
        }
        let t = std::time::Duration::from_secs(10);
        use tokio::time::timeout;
        assert_eq!(
            within("query")(timeout(t, query(&pool, read)).await).err(),
            Some(refused())
        );
        assert_eq!(
            within("query_with")(timeout(t, query_with(&pool, read, &[])).await).err(),
            Some(refused())
        );
        assert_eq!(
            within("query_capped")(timeout(t, query_capped(&pool, read, &[], 10, 100)).await).err(),
            Some(refused())
        );
        assert_eq!(
            within("execute")(timeout(t, execute(&pool, write)).await).err(),
            Some(refused())
        );
        let mut out = Vec::new();
        assert_eq!(
            within("export")(
                timeout(t, export(&pool, read, &[], ExportFormat::Csv, &mut out)).await
            )
            .err(),
            Some(refused())
        );
        assert_eq!(
            within("execute_all_checked")(
                timeout(
                    t,
                    execute_all_checked(
                        &pool,
                        &[CheckedStatement {
                            sql: write.into(),
                            params: vec![],
                            expected_rows: 1,
                        }]
                    )
                )
                .await
            )
            .err(),
            Some((Some(0), refused()))
        );
        let mut conn = pool.acquire().await.unwrap();
        assert_eq!(
            within("query_capped_conn")(
                timeout(t, query_capped_conn(&mut conn, read, 10, 100)).await
            )
            .err(),
            Some(refused())
        );
        assert_eq!(
            within("execute_conn")(timeout(t, execute_conn(&mut conn, write)).await).err(),
            Some(refused())
        );
        assert_eq!(
            within("execute_with_conn")(timeout(t, execute_with_conn(&mut conn, write, &[])).await)
                .err(),
            Some(refused())
        );
        drop(conn);
        let rows = query(&pool, "SELECT count(*) FROM t").await.unwrap();
        assert_eq!(rows.rows, vec![vec![Value::Integer(0)]]);
    }

    /// Every shipped file under `src/`, as `(path relative to src, contents)`.
    fn all_sources() -> Vec<(String, String)> {
        fn walk(dir: &Path, root: &Path, out: &mut Vec<(String, String)>) {
            for entry in std::fs::read_dir(dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    walk(&path, root, out);
                } else if path.extension().is_some_and(|e| e == "rs") {
                    let rel = path
                        .strip_prefix(root)
                        .unwrap()
                        .to_string_lossy()
                        .replace('\\', "/");
                    out.push((rel, std::fs::read_to_string(&path).unwrap()));
                }
            }
        }
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut out = Vec::new();
        walk(&root, &root, &mut out);
        out
    }

    /// The same detector over every other shipped file, so a SQLite path
    /// written outside this file — the read-only run in `registry.rs`, say —
    /// cannot slip past [`SqlText`] either. Three files are exempt, each for
    /// a reason:
    ///
    /// - `db/postgres.rs`: sqlx-postgres sends a statement whole, with no
    ///   prepare loop to stall in, and a Postgres-wire server has no NUL in
    ///   its names to build text from.
    /// - `db/sqlx_common.rs`: the shared helpers, generic over the driver.
    ///   The ones that take `&str` are off [`COMMON_ALLOWED`], so only the
    ///   door and `postgres.rs` call them; the generic paths on it
    ///   ([`COMMON_GENERIC`]) take text as a backend's `Text`, a [`SqlText`]
    ///   on SQLite.
    /// - `history.rs`: hubro's own history database, which is SQLite — so
    ///   it may reach the driver only by `sqlx::query(` or
    ///   `sqlx::query_scalar(` with a string literal, or the setup loop over
    ///   an array of them. What the user ran is stored there as a bound value.
    #[test]
    fn only_known_files_hand_sql_text_to_sqlx() {
        let mut reaching = Vec::new();
        for (path, source) in all_sources() {
            let code = match path.as_str() {
                "db/sqlite.rs" => sqlite_outside_the_door(),
                _ => shipped_code(&source).join("\n"),
            };
            let found = reaches_driver(&code);
            match path.as_str() {
                "db/postgres.rs" | "db/sqlx_common.rs" => reaching.push(path),
                "history.rs" => {
                    for (at, what) in &found {
                        let call = &code[*at..];
                        let argument = ["sqlx::query(", "sqlx::query_scalar("]
                            .iter()
                            .find_map(|call_of| call.strip_prefix(call_of))
                            .map(str::trim_start);
                        let literal = argument.is_some_and(|argument| {
                            argument.starts_with('"')
                                || (argument.starts_with("statement)")
                                    && code.contains("for statement in [\n"))
                        });
                        assert!(literal, "history.rs reaches the driver by `{what}`");
                    }
                    assert!(!found.is_empty(), "history's statements were not found");
                    reaching.push(path);
                }
                _ => assert!(
                    found.is_empty(),
                    "{path} hands text to the driver past `SqlText`: {found:?}"
                ),
            }
        }
        reaching.sort();
        assert_eq!(
            reaching,
            ["db/postgres.rs", "db/sqlx_common.rs", "history.rs"]
        );
    }

    fn key_column(name: &str, type_name: &str, pk: Option<u32>) -> ColumnMeta {
        ColumnMeta {
            name: name.into(),
            type_name: type_name.into(),
            nullable: false,
            primary_key_position: pk,
            default: None,
            generated: Generated::Never,
            type_detail: TypeDetail::Plain,
        }
    }

    #[test]
    fn the_rowid_alias_is_the_lone_key_column_without_a_pk_index() {
        let columns = vec![
            key_column("x", "TEXT", None),
            key_column("id", "INTEGER", Some(1)),
        ];
        assert_eq!(rowid_alias(true, &columns, false), Some(1));
        // A pk index means the key is stored apart from the rowid: a
        // `WITHOUT ROWID` table, a column-constraint `DESC`, a non-INTEGER
        // type. The type is not consulted either way.
        assert_eq!(rowid_alias(true, &columns, true), None);
        let int_key = vec![key_column("id", "INT", Some(1))];
        assert_eq!(rowid_alias(true, &int_key, false), Some(0));
        assert_eq!(rowid_alias(true, &int_key, true), None);
    }

    #[test]
    fn no_rowid_alias_without_exactly_one_key_column_in_an_ordinary_table() {
        let composite = vec![
            key_column("a", "INTEGER", Some(1)),
            key_column("b", "INTEGER", Some(2)),
        ];
        assert_eq!(rowid_alias(true, &composite, false), None);
        let keyless = vec![key_column("a", "INTEGER", None)];
        assert_eq!(rowid_alias(true, &keyless, false), None);
        assert_eq!(rowid_alias(true, &[], false), None);
        // A view or virtual table has no index list, so the missing pk index
        // proves nothing about a key it declares.
        let keyed = vec![key_column("id", "INTEGER", Some(1))];
        assert_eq!(rowid_alias(false, &keyed, false), None);
    }

    #[test]
    fn only_pk_and_unique_origins_are_constraint_indexes() {
        assert_eq!(index_constraint("pk"), Some(IndexConstraint::PrimaryKey));
        assert_eq!(index_constraint("u"), Some(IndexConstraint::Unique));
        assert_eq!(index_constraint("c"), None);
    }

    /// A one-connection pool, so a TEMP table or an ATTACH made on it is seen
    /// by the introspection that follows — both are per-connection.
    async fn one_connection() -> SqlitePool {
        SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap()
    }

    async fn run(pool: &SqlitePool, sql: &str) {
        SqlText::new(sql).unwrap().execute(pool).await.unwrap();
    }

    #[tokio::test]
    async fn a_temp_table_of_the_same_name_does_not_lend_its_shape_to_the_main_one() {
        // Unqualified, the pragmas resolve a name temp-first. Measured: with
        // `main.s` WITHOUT ROWID and `temp.s` a rowid table, `pragma_index_list('s')`
        // describes the temp one — no pk index — so `s` was declared the rowid
        // alias, and an import's blank id was left out and refused NOT NULL.
        let pool = one_connection().await;
        run(
            &pool,
            "CREATE TABLE s (id INTEGER PRIMARY KEY NOT NULL, x TEXT REFERENCES p_main (id)) \
             WITHOUT ROWID",
        )
        .await;
        run(
            &pool,
            "CREATE TEMP TABLE s (id INTEGER PRIMARY KEY, \
             only_in_temp TEXT REFERENCES p_temp (id))",
        )
        .await;
        let tables = introspect(&pool).await.unwrap();
        let s: Vec<&TableMeta> = tables.iter().filter(|t| t.name == "s").collect();
        assert_eq!(s.len(), 1, "{tables:?}");
        let columns: Vec<&str> = s[0].columns.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(columns, ["id", "x"], "main's columns, not temp's");
        assert_eq!(s[0].columns[0].generated, Generated::Never);
        assert_eq!(
            s[0].indexes
                .iter()
                .map(|i| i.constraint)
                .collect::<Vec<_>>(),
            [Some(IndexConstraint::PrimaryKey)]
        );
        let foreign_keys: Vec<(&str, &[String])> = s[0]
            .foreign_keys
            .iter()
            .map(|fk| (fk.referenced_table.as_str(), fk.columns.as_slice()))
            .collect();
        assert_eq!(
            foreign_keys,
            [("p_main", &["x".to_string()][..])],
            "main's foreign keys, not temp's"
        );
    }

    #[tokio::test]
    async fn only_the_main_schema_is_introspected() {
        // TEMP and ATTACHed tables are not listed at all, so their keys never
        // reach the rowid decision.
        let pool = one_connection().await;
        run(&pool, "CREATE TABLE kept (id INTEGER PRIMARY KEY)").await;
        run(&pool, "CREATE TEMP TABLE scratch (id INTEGER PRIMARY KEY)").await;
        run(&pool, "ATTACH DATABASE ':memory:' AS aux").await;
        run(&pool, "CREATE TABLE aux.elsewhere (id INTEGER PRIMARY KEY)").await;
        let tables = introspect(&pool).await.unwrap();
        let names: Vec<&str> = tables.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, ["kept"]);
        assert_eq!(tables[0].columns[0].generated, Generated::RowidAlias);
    }
}
