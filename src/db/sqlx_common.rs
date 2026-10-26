//! Execution scaffolding shared by the two sqlx backends (SQLite, Postgres).
//!
//! The two differ in a handful of things — the `Database` type parameter, how
//! SQL text becomes a statement (SQLite's passes its NUL door first), how a
//! parameter is bound, how a cell is decoded, and how a driver error is mapped
//! (SQLite has no error position to enrich with; Postgres appends
//! line/column). [`SqlxBackend`] names those, and everything between "here is
//! the text and its parameters" and "the caller gets a [`QueryResult`]" is
//! written once here, the checked write ([`execute_all_checked`]) included.
//!
//! Generic functions rather than a macro: the backend modules stay ordinary
//! Rust that a reader can follow and a compiler error can point into, and the
//! shared code has one signature to read instead of an expansion to imagine.
//!
//! **No SQLite text reaches the driver here unchecked.** The generic paths
//! take a [`SqlxBackend::Text`], never a `&str`, and SQLite's is its
//! `SqlText`, which only its door builds. So the type, not a reading of this
//! file, is what keeps a generic path from handing sqlx-sqlite a NUL —
//! `every_sqlite_entry_point_refuses_a_nul` (`sqlite.rs`) runs each of them.

use std::io::Write;

use futures_util::{Stream, TryStreamExt as _};
use sqlx::{Column as _, ColumnIndex, Database, Decode, Executor, IntoArguments, Row, Type};

use super::error::DbError;
use super::export::{export_io_err, ExportFormat, ExportSink};
use super::staged::CheckedStatement;
use super::value::{cap_value, Capped, ColumnInfo, QueryResult, RestOfRead, Value};

/// The affected-row count of a driver's execute result. sqlx has no trait for
/// this — `rows_affected` is an inherent method on each backend's
/// `QueryResult` — so this is the one bridge the generic [`execute`] needs.
pub(super) trait AffectedRows {
    fn affected_rows(&self) -> u64;
}

impl AffectedRows for sqlx::sqlite::SqliteQueryResult {
    fn affected_rows(&self) -> u64 {
        self.rows_affected()
    }
}

impl AffectedRows for sqlx::postgres::PgQueryResult {
    fn affected_rows(&self) -> u64 {
        self.rows_affected()
    }
}

/// Reads one typed column out of an introspection row by name, naming the
/// column in the error (a decode failure otherwise says only "mismatched
/// types" with no hint which of two dozen catalog columns it was).
/// `T` is first so a call site can name just the decoded type
/// (`get::<Option<String>, _>(row, "…")`) and leave the row type inferred.
pub(super) fn get<'r, T, R>(row: &'r R, column: &'r str) -> Result<T, DbError>
where
    R: Row,
    T: Decode<'r, R::Database> + Type<R::Database>,
    &'r str: ColumnIndex<R>,
{
    row.try_get(column)
        .map_err(|e| DbError::Introspect(format!("column {column}: {e}")))
}

/// Executes a statement without decoding rows, returning the driver's
/// affected-row count. Takes any sqlx executor, so this is both the pool path
/// and the single-connection path (a statement inside an atomically-wrapped
/// script).
///
/// Goes through `sqlx::query` rather than handing the executor the bare `&str`
/// on purpose: the former is a prepared statement, the latter would be the
/// simple/unprepared protocol, which accepts several statements in one call.
pub(super) async fn execute<'c, 'q, DB, E>(
    executor: E,
    sql: &'q str,
    map_err: impl Fn(sqlx::Error) -> DbError,
) -> Result<u64, DbError>
where
    DB: Database,
    E: Executor<'c, Database = DB>,
    <DB as Database>::Arguments<'q>: IntoArguments<'q, DB>,
    <DB as Database>::QueryResult: AffectedRows,
{
    sqlx::query(sql)
        .execute(executor)
        .await
        .map(|done| done.affected_rows())
        .map_err(map_err)
}

/// A prepared statement over a backend's text, with its arguments.
pub(super) type Query<'q, DB> = sqlx::query::Query<'q, DB, <DB as Database>::Arguments<'q>>;

/// What one sqlx backend supplies to the generic paths below. Implemented by
/// a zero-sized type per backend (`sqlite::SqliteBackend`,
/// `postgres::PgBackend`), so a generic path is named
/// `query_with::<SqliteBackend, _>` and costs nothing at run
/// time.
pub(super) trait SqlxBackend {
    type DB: Database;

    /// SQL text this backend lets reach its driver. On SQLite, `SqlText`,
    /// which refuses a NUL; on Postgres, the text itself.
    type Text<'q>: Copy;

    /// `sql` as [`Self::Text`], or the error that keeps it from the driver.
    /// The one way the generic paths' callers get a `Text`.
    fn prepare(sql: &str) -> Result<Self::Text<'_>, DbError>;

    /// The text, for a describe or an error message.
    fn sql<'q>(text: Self::Text<'q>) -> &'q str;

    /// A prepared (not simple-protocol) statement over `text`.
    fn query<'q>(text: Self::Text<'q>) -> Query<'q, Self::DB>;

    /// Binds one backend-neutral parameter.
    fn bind<'q>(query: Query<'q, Self::DB>, value: &Value) -> Query<'q, Self::DB>;

    /// Decodes one cell of a fetched row.
    fn decode(row: &<Self::DB as Database>::Row, idx: usize) -> Result<Value, DbError>;

    /// Maps a driver error from running `sql`.
    fn map_err(err: sqlx::Error, sql: &str) -> DbError;
}

/// Binds backend-neutral [`Value`] parameters onto a prepared query.
fn bind_params<'q, B: SqlxBackend>(
    mut query: Query<'q, B::DB>,
    params: &[Value],
) -> Query<'q, B::DB> {
    for param in params {
        query = B::bind(query, param);
    }
    query
}

/// Runs a query and reads its whole result, recovering a zero-row result's
/// headers. Uncapped, so only tests read through it
/// (`DbPool::query_unbounded`).
pub(super) async fn query_all<'q, B>(
    pool: &sqlx::Pool<B::DB>,
    text: B::Text<'q>,
) -> Result<QueryResult, DbError>
where
    B: SqlxBackend,
    for<'p> &'p sqlx::Pool<B::DB>: Executor<'p, Database = B::DB>,
    <B::DB as Database>::Arguments<'q>: IntoArguments<'q, B::DB>,
{
    let mut result = query_with::<B, _>(pool, text, &[]).await?;
    fill_headers(&mut result, pool, B::sql(text)).await;
    Ok(result)
}

/// Runs a query with bound parameters and reads its whole result.
///
/// No header recovery on a zero-row result here, unlike [`query_all`] and
/// [`query_capped`]: every caller of this builds its own projection (a page
/// fetch, a cell fetch, a catalog read for DDL) and so already knows the
/// columns, and nothing downstream reads them off the result. Describing the
/// statement to fill in headers no one looks at would put an extra round trip
/// on each of those — two on Postgres, whose `describe` also issues a
/// `pg_attribute` query — and empty catalog reads are ordinary (a table with
/// no table-level constraints, no non-constraint indexes).
pub(super) async fn query_with<'c, 'q, B, E>(
    executor: E,
    text: B::Text<'q>,
    params: &[Value],
) -> Result<QueryResult, DbError>
where
    B: SqlxBackend,
    E: Executor<'c, Database = B::DB>,
    <B::DB as Database>::Arguments<'q>: IntoArguments<'q, B::DB>,
{
    let stream = bind_params::<B>(B::query(text), params).fetch(executor);
    collect_all(stream, B::decode, |e| B::map_err(e, B::sql(text))).await
}

/// Streams a query one row at a time (`fetch`, not `fetch_all`), decoding and
/// retaining at most `max_rows` rows and capping each cell to `cell_cap`
/// bytes, so neither the row count nor a pathologically large cell can make
/// the free-form query path scale with table or value size. Returns the
/// (bounded) result and whether more rows existed beyond the cap. No header
/// recovery — [`query_capped`] adds it on a pool, and a caller holding one
/// connection describes on it once the stream is gone.
pub(super) async fn query_capped_rows<'c, 'q, B, E>(
    executor: E,
    text: B::Text<'q>,
    params: &[Value],
    max_rows: u64,
    cell_cap: usize,
) -> Result<(QueryResult, bool), DbError>
where
    B: SqlxBackend,
    E: Executor<'c, Database = B::DB>,
    <B::DB as Database>::Arguments<'q>: IntoArguments<'q, B::DB>,
{
    let stream = bind_params::<B>(B::query(text), params).fetch(executor);
    collect_capped(stream, max_rows, cell_cap, B::decode, |e| {
        B::map_err(e, B::sql(text))
    })
    .await
}

/// [`query_capped_rows`] on a pool, with a zero-row result's headers
/// recovered. The describe takes a connection of its own: a read cut short at
/// a cap of 0 keeps no row and so no header, and on the read's own connection
/// the describe would wait for the rest of its result first.
pub(super) async fn query_capped<'q, B>(
    pool: &sqlx::Pool<B::DB>,
    text: B::Text<'q>,
    params: &[Value],
    max_rows: u64,
    cell_cap: usize,
) -> Result<(QueryResult, bool), DbError>
where
    B: SqlxBackend,
    for<'p> &'p sqlx::Pool<B::DB>: Executor<'p, Database = B::DB>,
    <B::DB as Database>::Arguments<'q>: IntoArguments<'q, B::DB>,
{
    let (mut result, truncated) =
        query_capped_rows::<B, _>(pool, text, params, max_rows, cell_cap).await?;
    fill_headers(&mut result, pool, B::sql(text)).await;
    Ok((result, truncated))
}

/// Runs a statement with bound parameters without decoding rows, returning
/// the driver's affected-row count — the write path for a batch of imported
/// rows, which runs inside the caller's transaction and must not interpolate
/// its values into the SQL.
pub(super) async fn execute_with<'c, 'q, B, E>(
    executor: E,
    text: B::Text<'q>,
    params: &[Value],
) -> Result<u64, DbError>
where
    B: SqlxBackend,
    E: Executor<'c, Database = B::DB>,
    <B::DB as Database>::Arguments<'q>: IntoArguments<'q, B::DB>,
    <B::DB as Database>::QueryResult: AffectedRows,
{
    bind_params::<B>(B::query(text), params)
        .execute(executor)
        .await
        .map(|done| done.affected_rows())
        .map_err(|e| B::map_err(e, B::sql(text)))
}

/// Streams a query to `out` in the given format, pulling rows one at a time
/// (`fetch`, not `fetch_all`) and writing each incrementally — peak memory is
/// one decoded row plus the writer's buffer. Returns the number of data rows
/// written. When the result is empty the column names come from a statement
/// describe, on the same connection, so the header (CSV) / empty array (JSON)
/// still reflect the query.
pub(super) async fn export_query<'q, B>(
    conn: &mut <B::DB as Database>::Connection,
    text: B::Text<'q>,
    params: &[Value],
    format: ExportFormat,
    out: &mut impl Write,
) -> Result<u64, DbError>
where
    B: SqlxBackend,
    for<'e> &'e mut <B::DB as Database>::Connection: Executor<'e, Database = B::DB>,
    <B::DB as Database>::Arguments<'q>: IntoArguments<'q, B::DB>,
{
    let sql = B::sql(text);
    let stream = bind_params::<B>(B::query(text), params).fetch(&mut *conn);
    if let Some(rows) =
        export_stream(stream, format, out, B::decode, |e| B::map_err(e, sql)).await?
    {
        return Ok(rows);
    }
    // No rows streamed, so no row carried the column names: describe the
    // statement instead. This one propagates its failure — an export whose
    // header cannot be determined is an empty file that says nothing.
    let columns = describe_columns(&mut *conn, sql)
        .await
        .map_err(|e| B::map_err(e, sql))?;
    export_empty(format, columns, out)?;
    Ok(0)
}

/// Executes parameterized writes inside ONE transaction, committing only
/// when every statement affected exactly its `expected_rows` rows. Any SQL
/// error or count mismatch rolls the whole batch back; the error carries the
/// index of the failing statement (`None` for begin/commit failures, which
/// belong to no statement). This is the safety net for row edits: a WHERE
/// clause that unexpectedly matches more (or fewer) rows than the one being
/// edited must never commit — and with staged edits, neither may
/// any sibling change in the same batch.
///
/// Every statement is prepared ([`SqlxBackend::prepare`]) before the
/// transaction opens, so one refused there — SQLite's NUL — leaves nothing
/// to roll back and none of the batch run.
pub(super) async fn execute_all_checked<B>(
    pool: &sqlx::Pool<B::DB>,
    statements: &[CheckedStatement],
) -> Result<(), (Option<usize>, DbError)>
where
    B: SqlxBackend,
    for<'e> &'e mut <B::DB as Database>::Connection: Executor<'e, Database = B::DB>,
    for<'q> <B::DB as Database>::Arguments<'q>: IntoArguments<'q, B::DB>,
    <B::DB as Database>::QueryResult: AffectedRows,
{
    let texts = statements
        .iter()
        .enumerate()
        .map(|(index, statement)| B::prepare(&statement.sql).map_err(|e| (Some(index), e)))
        .collect::<Result<Vec<_>, _>>()?;
    let mut tx = pool
        .begin()
        .await
        .map_err(|e| (None, DbError::Query(e.to_string())))?;
    for (index, (statement, text)) in statements.iter().zip(texts).enumerate() {
        let result = bind_params::<B>(B::query(text), &statement.params)
            .execute(&mut *tx)
            .await;
        // Dropping the transaction would roll back too; do it explicitly and
        // ignore secondary errors — the original failure is what the caller
        // needs.
        let affected = match result {
            Ok(done) => done.affected_rows(),
            Err(e) => {
                let _ = tx.rollback().await;
                return Err((Some(index), B::map_err(e, B::sql(text))));
            }
        };
        if let Some(mismatch) = row_count_mismatch(affected, statement.expected_rows) {
            let _ = tx.rollback().await;
            return Err((Some(index), mismatch));
        }
    }
    tx.commit()
        .await
        .map_err(|e| (None, DbError::Query(e.to_string())))
}

/// The checked write's decision for one statement, over plain counts: an
/// error — and the batch rolled back — unless the statement changed exactly
/// the rows it was built to change.
fn row_count_mismatch(affected: u64, expected: u64) -> Option<DbError> {
    (affected != expected).then(|| DbError::row_count_mismatch(affected, expected))
}

/// A connection for one script run, checked out of `pool` and marked to be
/// closed rather than returned to it when it is let go — on every path,
/// a cancelled run's drop included. Whatever the script left on its session
/// goes with it: see [`ScriptConn`](super::registry::ScriptConn).
///
/// The bound on a close on drop: sqlx gives it five seconds
/// (`CLOSE_ON_DROP_TIMEOUT`), then frees the pool slot whether or not the
/// close finished. On SQLite a cancelled statement runs on in the worker
/// thread past that, holding the write lock, and the connection only closes
/// — rolling back what the run left uncommitted, measured after a 7 s
/// statement cancelled at 0.3 s — once it ends. The freed slot is not what an
/// edit waits on meanwhile: any connection's edit waits SQLite's own five
/// second busy timeout for the lock and then fails with "database is locked",
/// measured the same on both paths before runs closed their connections. The
/// slot matters only when every one of the pool's connections (ten for
/// SQLite) is busy at once.
pub(super) async fn acquire_for_run<DB: Database>(
    pool: &sqlx::Pool<DB>,
) -> Result<sqlx::pool::PoolConnection<DB>, DbError> {
    let mut conn = pool
        .acquire()
        .await
        .map_err(|e| DbError::Query(e.to_string()))?;
    conn.close_on_drop();
    Ok(conn)
}

/// A script transaction on a connection from [`acquire_for_run`], so the
/// connection closes once the transaction is resolved. The transaction owns
/// the connection (as `Pool::begin`'s does), which is why this goes through
/// `Transaction::begin` rather than `Pool::begin`: the latter returns a
/// connection to the pool, and nothing reaches the one inside its handle.
pub(super) async fn begin_for_run<DB: Database>(
    pool: &sqlx::Pool<DB>,
) -> Result<sqlx::Transaction<'static, DB>, DbError> {
    let conn = acquire_for_run(pool).await?;
    sqlx::Transaction::begin(conn, None)
        .await
        .map_err(|e| DbError::Query(e.to_string()))
}

/// Commits a script transaction — its statements all take effect.
pub(super) async fn commit_tx<DB: Database>(tx: sqlx::Transaction<'_, DB>) -> Result<(), DbError> {
    tx.commit().await.map_err(|e| DbError::Query(e.to_string()))
}

/// Rolls a script transaction back. Best-effort: a rollback failure leaves
/// nothing committed anyway (the transaction also rolls back on drop).
pub(super) async fn rollback_tx<DB: Database>(tx: sqlx::Transaction<'_, DB>) {
    let _ = tx.rollback().await;
}

/// The result columns as the driver reported them on a fetched row.
fn columns_of<R: Row>(row: &R) -> Vec<ColumnInfo> {
    row.columns()
        .iter()
        .map(|c| ColumnInfo {
            name: c.name().to_string(),
        })
        .collect()
}

/// Decodes every cell of one fetched row into the backend-neutral [`Value`]
/// model, capping each to `cell_cap` bytes.
fn decode_row<R: Row>(
    row: &R,
    cell_cap: usize,
    decode_value: &impl Fn(&R, usize) -> Result<Value, DbError>,
) -> Result<Vec<Value>, DbError> {
    let mut values = Vec::with_capacity(row.columns().len());
    for idx in 0..row.columns().len() {
        values.push(cap_value(decode_value(row, idx)?, cell_cap));
    }
    Ok(values)
}

/// Drains a row stream into a [`QueryResult`], decoding every row and capping
/// nothing — the buffered query path, where the caller has already bounded the
/// result some other way (a `LIMIT`-ed page fetch, a catalog query).
pub(super) async fn collect_all<S, R>(
    stream: S,
    decode_value: impl Fn(&R, usize) -> Result<Value, DbError>,
    map_err: impl Fn(sqlx::Error) -> DbError,
) -> Result<QueryResult, DbError>
where
    S: Stream<Item = Result<R, sqlx::Error>> + Unpin,
    R: Row,
{
    // No caps: `usize::MAX`/`u64::MAX` make both bounds unreachable, so this
    // is the capped drain with its two limiters switched off rather than a
    // second copy of the same loop.
    let (result, _) = collect_capped(stream, u64::MAX, usize::MAX, decode_value, map_err).await?;
    Ok(result)
}

/// Drains a row stream into a bounded [`QueryResult`], keeping at most
/// `max_rows` rows and capping each cell to `cell_cap` bytes; the bool is
/// whether rows existed past the cap. The rest of a cut-short result is left
/// unread ([`Rest::Leave`]) — see [`collect_capped_then`] for the others.
pub(super) async fn collect_capped<S, R>(
    stream: S,
    max_rows: u64,
    cell_cap: usize,
    decode_value: impl Fn(&R, usize) -> Result<Value, DbError>,
    map_err: impl Fn(sqlx::Error) -> DbError,
) -> Result<(QueryResult, bool), DbError>
where
    S: Stream<Item = Result<R, sqlx::Error>> + Unpin,
    R: Row,
{
    let capped = collect_capped_then(stream, max_rows, cell_cap, decode_value, map_err, || {
        std::future::ready(Rest::Leave)
    })
    .await?;
    Ok((capped.result, capped.truncated))
}

/// What a read the row cap cut short does with the rest of its result,
/// decided at the moment the cap is reached ([`collect_capped_then`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Rest {
    /// Left unread. The connection cannot be asked anything until the rest
    /// has arrived, so the caller closes it, or has it read elsewhere.
    Leave,
    /// Read to its end here and thrown away, giving the thread a [`turn`]
    /// now and then.
    Read,
    /// A cancel has been sent for the statement: read, the same way, until
    /// the server reports it cancelled (or the statement ends first).
    /// `timeout_off` is whether the session's `statement_timeout` was off when
    /// last asked, `None` if not asked ([`cancel_landed`]).
    Cancelled { timeout_off: Option<bool> },
}

/// How often, in discarded rows, reading the rest of a cut-short result
/// checks whether it is time for a [`turn`].
pub(super) const DRAIN_CHECK_ROWS: u64 = 1024;

/// How long reading the rest of a cut-short result goes between [`turn`]s.
const DRAIN_TURN_EVERY: std::time::Duration = std::time::Duration::from_millis(20);

/// Gives everything else on the thread a turn while the rest of a cut-short
/// result is read.
///
/// The turns are the point. Over localhost a row is always waiting, so the
/// driver's read never returns `Pending`: measured before, the rest of 200M
/// rows was read in one 44 s poll, during which a timer beside the run did not
/// fire and `abort()` on its task did not take effect.
///
/// A timer rather than a wake-up: the SQL tab's run is a Dioxus task, and
/// Dioxus polls a task that woke itself again before returning to the window's
/// event loop. Measured in the app with an immediate wake every 1024 rows, the
/// run went on, and a click on Cancel was not seen until the rest had been
/// read. A 1 ms timer every 20 ms leaves the loop free in between, for about
/// 5% of the reading time.
async fn turn() {
    tokio::time::sleep(std::time::Duration::from_millis(1)).await;
}

/// SQLSTATE `query_canceled`: what a cancelled Postgres-wire statement fails
/// with (measured the same on PostgreSQL, YugabyteDB and CockroachDB).
pub(super) const QUERY_CANCELED: &str = "57014";

/// Whether a `57014` that ends a statement hubro has sent a cancel for is
/// that cancel, and not the session's `statement_timeout` firing between
/// the cap and the cancel — the same code, distinguished by the server only
/// in its message.
///
/// The message is matched where it is known (PostgreSQL's, CockroachDB's —
/// which does not translate its messages). `lc_messages` translates
/// PostgreSQL's, so an unknown message is hubro's cancel only if the session
/// had no `statement_timeout` to fire (`timeout_off`, asked before the read);
/// otherwise, or not knowing, it is reported as the statement's error — a
/// cancel shown as an error, rather than a timeout shown as a cancel.
pub(super) fn cancel_landed(message: &str, timeout_off: Option<bool>) -> bool {
    if message.contains("statement timeout") {
        return false;
    }
    if message == "canceling statement due to user request" || message == "query execution canceled"
    {
        return true;
    }
    timeout_off == Some(true)
}

/// [`collect_capped`], with what happens to the rest of a cut-short result
/// decided by `at_cap` when the cap is reached — while the statement is
/// still running, which is when a cancel can still stop it. Not called for a
/// result that fits.
pub(super) async fn collect_capped_then<S, R, F, Fut>(
    mut stream: S,
    max_rows: u64,
    cell_cap: usize,
    decode_value: impl Fn(&R, usize) -> Result<Value, DbError>,
    map_err: impl Fn(sqlx::Error) -> DbError,
    at_cap: F,
) -> Result<Capped, DbError>
where
    S: Stream<Item = Result<R, sqlx::Error>> + Unpin,
    R: Row,
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = Rest>,
{
    let mut columns: Vec<ColumnInfo> = Vec::new();
    let mut out_rows: Vec<Vec<Value>> = Vec::new();
    let mut truncated = false;
    while let Some(row) = stream.try_next().await.map_err(&map_err)? {
        // The cap+1'th row that reaches us proves there is more; stop before
        // decoding it so exactly `max_rows` rows are retained.
        if out_rows.len() as u64 >= max_rows {
            truncated = true;
            break;
        }
        if columns.is_empty() {
            columns = columns_of(&row);
        }
        out_rows.push(decode_row(&row, cell_cap, &decode_value)?);
    }
    let landed = if truncated {
        let rest = at_cap().await;
        read_rest(&mut stream, rest, &map_err).await?
    } else {
        false
    };
    Ok(Capped {
        result: QueryResult {
            columns,
            rows: out_rows,
        },
        truncated,
        rest: if landed {
            RestOfRead::Stopped
        } else {
            RestOfRead::Ran
        },
    })
}

/// Reads the rest of a cut-short result as `rest` says, and reports whether
/// the statement ended cancelled. An error is the statement's own — a write
/// past the cap that failed, say — and fails the read, which is where it
/// belongs: left for the connection's next question, it was reported as that
/// question's failure.
async fn read_rest<S, R>(
    stream: &mut S,
    rest: Rest,
    map_err: &impl Fn(sqlx::Error) -> DbError,
) -> Result<bool, DbError>
where
    S: Stream<Item = Result<R, sqlx::Error>> + Unpin,
{
    if rest == Rest::Leave {
        return Ok(false);
    }
    let mut discarded = 0u64;
    let mut since_turn = std::time::Instant::now();
    loop {
        match stream.try_next().await {
            Ok(Some(_)) => {
                discarded += 1;
                if discarded.is_multiple_of(DRAIN_CHECK_ROWS)
                    && since_turn.elapsed() >= DRAIN_TURN_EVERY
                {
                    turn().await;
                    since_turn = std::time::Instant::now();
                }
            }
            Ok(None) => return Ok(false),
            Err(sqlx::Error::Database(e))
                if e.code().as_deref() == Some(QUERY_CANCELED)
                    && matches!(rest, Rest::Cancelled { timeout_off }
                        if cancel_landed(e.message(), timeout_off)) =>
            {
                return Ok(true)
            }
            Err(e) => return Err(map_err(e)),
        }
    }
}

/// Streams a query to `out` in the given format, pulling rows one at a time
/// and writing each incrementally — peak memory is one decoded row plus the
/// writer's buffer. Returns the number of data rows written, or `None` when
/// the stream held no rows at all and nothing was written: the columns are
/// only knowable from a row here, so the caller finishes that case with
/// [`describe_columns`] + [`export_empty`] once the stream (and its borrow of
/// the connection) is gone.
pub(super) async fn export_stream<S, R>(
    mut stream: S,
    format: ExportFormat,
    out: &mut impl Write,
    decode_value: impl Fn(&R, usize) -> Result<Value, DbError>,
    map_err: impl Fn(sqlx::Error) -> DbError,
) -> Result<Option<u64>, DbError>
where
    S: Stream<Item = Result<R, sqlx::Error>> + Unpin,
    R: Row,
{
    let mut sink: Option<ExportSink> = None;
    let mut rows = 0u64;
    while let Some(row) = stream.try_next().await.map_err(&map_err)? {
        let sink = match sink.as_mut() {
            Some(sink) => sink,
            None => {
                let columns = row.columns().iter().map(|c| c.name().to_string()).collect();
                let mut new_sink = ExportSink::new(format, columns);
                new_sink.begin(out).map_err(export_io_err)?;
                sink.insert(new_sink)
            }
        };
        // Export is not a display path: cells go out whole, uncapped.
        let values = decode_row(&row, usize::MAX, &decode_value)?;
        sink.write_row(&values, out).map_err(export_io_err)?;
        rows += 1;
    }
    match sink.as_mut() {
        Some(sink) => {
            sink.end(out).map_err(export_io_err)?;
            Ok(Some(rows))
        }
        None => Ok(None),
    }
}

/// Writes an export with no data rows, so an empty result still carries the
/// column header (CSV) or a well-formed empty array (JSON).
pub(super) fn export_empty(
    format: ExportFormat,
    columns: Vec<String>,
    out: &mut impl Write,
) -> Result<(), DbError> {
    let mut sink = ExportSink::new(format, columns);
    sink.begin(out).map_err(export_io_err)?;
    sink.end(out).map_err(export_io_err)
}

/// Column names of a prepared statement, without running it. This is how a
/// zero-row result still knows its columns: the sqlx row stream reports them
/// per row, so with no rows there is nothing to read them off (unlike TDS,
/// which sends result-set metadata regardless) — see [`fill_headers`].
pub(super) async fn describe_columns<'c, 'q, E>(
    executor: E,
    sql: &'q str,
) -> Result<Vec<String>, sqlx::Error>
where
    E: Executor<'c>,
    'c: 'q,
{
    let described = executor.describe(sql).await?;
    Ok(described
        .columns()
        .iter()
        .map(|c| c.name().to_string())
        .collect())
}

/// Gives a result that streamed no rows the headers it would have had:
/// an empty `SELECT` shows its column names rather than nothing,
/// matching the SQL Server backend, which gets them from TDS metadata for
/// free. Called after the row stream is dropped, since the stream holds the
/// borrow of the executor this needs.
///
/// Deliberately best-effort — a describe that fails leaves the columns empty
/// instead of erroring. The statement has already run successfully at this
/// point, and re-preparing it can fail for reasons that say nothing about that
/// run: a `DROP TABLE t` routed through the query path no longer has a `t` to
/// prepare against. Turning a completed statement into an error to explain a
/// missing header would be the worse trade.
pub(super) async fn fill_headers<'c, 'q, E>(result: &mut QueryResult, executor: E, sql: &'q str)
where
    E: Executor<'c>,
    'c: 'q,
{
    if !result.columns.is_empty() {
        return;
    }
    result.columns = describe_columns(executor, sql)
        .await
        .unwrap_or_default()
        .into_iter()
        .map(|name| ColumnInfo { name })
        .collect();
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The checked write's per-statement decision: only an exact count
    /// lets the batch go on — one row more or one fewer rolls it back.
    #[test]
    fn only_the_expected_row_count_lets_a_checked_write_commit() {
        assert_eq!(super::row_count_mismatch(1, 1), None);
        assert_eq!(super::row_count_mismatch(0, 0), None);
        for (affected, expected) in [(2, 1), (0, 1), (1, 0)] {
            assert_eq!(
                super::row_count_mismatch(affected, expected),
                Some(DbError::row_count_mismatch(affected, expected)),
                "{affected} of {expected}"
            );
        }
    }

    /// A server error with a SQLSTATE and message, as the driver hands one
    /// over.
    #[derive(Debug)]
    struct ServerError(&'static str, &'static str);

    impl std::fmt::Display for ServerError {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "{} ({})", self.1, self.0)
        }
    }

    impl std::error::Error for ServerError {}

    impl sqlx::error::DatabaseError for ServerError {
        fn message(&self) -> &str {
            self.1
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

    const USER_REQUEST: &str = "canceling statement due to user request";
    const TIMEOUT: &str = "canceling statement due to statement timeout";
    /// PostgreSQL's user-request message under `lc_messages = 'de_DE'`.
    const TRANSLATED: &str = "storniere Anfrage wegen Benutzeraufforderung";

    /// The rest of a result: `rows` rows, then `end` — `None` for the
    /// statement finishing, or the SQLSTATE and message it fails with.
    fn rest(
        rows: usize,
        end: Option<(&'static str, &'static str)>,
    ) -> impl Stream<Item = Result<(), sqlx::Error>> + Unpin {
        let rows = std::iter::repeat_with(|| Ok(())).take(rows);
        let end = end.map(|(code, message)| {
            Err(sqlx::Error::Database(Box::new(ServerError(code, message))))
        });
        futures_util::stream::iter(rows.chain(end))
    }

    fn read(
        rows: usize,
        end: Option<(&'static str, &'static str)>,
        how: Rest,
    ) -> Result<bool, DbError> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();
        runtime.block_on(read_rest(&mut rest(rows, end), how, &|e| {
            DbError::Query(e.to_string())
        }))
    }

    const CANCELLED: Rest = Rest::Cancelled { timeout_off: None };

    #[test]
    fn a_cancel_is_reported_only_when_the_server_says_the_statement_was_cancelled() {
        let cancelled = Some((QUERY_CANCELED, USER_REQUEST));
        // The cancel landed: the statement ended cancelled.
        assert_eq!(read(3, cancelled, CANCELLED).ok(), Some(true));
        // The cancel was taken but the statement finished first: nothing was
        // cancelled, and the read is reported as run to its end.
        assert_eq!(read(3, None, CANCELLED).ok(), Some(false));
        // Any other failure is the statement's own.
        assert!(read(3, Some(("23505", "duplicate key")), CANCELLED).is_err());
        // The session's `statement_timeout` firing between the cap and the
        // cancel: the same code, and the statement's error, not a cancel.
        assert!(read(3, Some((QUERY_CANCELED, TIMEOUT)), CANCELLED).is_err());
    }

    #[test]
    fn a_cancel_whose_message_is_translated_counts_only_when_no_timeout_could_fire() {
        let translated = Some((QUERY_CANCELED, TRANSLATED));
        let with = |timeout_off| Rest::Cancelled { timeout_off };
        assert_eq!(read(3, translated, with(Some(true))).ok(), Some(true));
        assert!(read(3, translated, with(Some(false))).is_err());
        assert!(read(3, translated, with(None)).is_err());
        // A known message decides whatever the setting.
        assert!(read(3, Some((QUERY_CANCELED, TIMEOUT)), with(Some(true))).is_err());
        assert_eq!(
            read(3, Some((QUERY_CANCELED, USER_REQUEST)), with(Some(false))).ok(),
            Some(true)
        );
        // CockroachDB's own, untranslated.
        assert!(cancel_landed("query execution canceled", None));
        assert!(!cancel_landed(
            "query execution canceled due to statement timeout",
            Some(true)
        ));
    }

    #[test]
    fn a_rest_read_to_its_end_reports_the_statements_failure() {
        assert_eq!(read(3, None, Rest::Read).ok(), Some(false));
        // A unique violation past the cap fails the read, rather than the
        // connection's next question or nothing at all.
        assert!(read(3, Some(("23505", "duplicate key")), Rest::Read).is_err());
        // Without a cancel of hubro's, a cancellation is someone else's — a
        // `statement_timeout`, say — and is an error like any other.
        assert!(read(3, Some((QUERY_CANCELED, USER_REQUEST)), Rest::Read).is_err());
        // Left unread: nothing is read, so nothing is reported.
        assert_eq!(
            read(3, Some(("23505", "duplicate key")), Rest::Leave).ok(),
            Some(false)
        );
    }
}
