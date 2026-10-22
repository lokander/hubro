//! Execution scaffolding shared by the two sqlx backends (SQLite, Postgres).
//!
//! The two differ in exactly three things — the `Database` type parameter, how
//! a cell is decoded, and how a driver error is mapped (SQLite has no error
//! position to enrich with; Postgres appends line/column) — so everything
//! between "the driver handed us a row stream" and "the caller gets a
//! [`QueryResult`]" is written once here and parameterized on those three.
//!
//! Generic functions rather than a macro: the backend modules stay ordinary
//! Rust that a reader can follow and a compiler error can point into, and the
//! shared code has one signature to read instead of an expansion to imagine.
//! The price is per-call closures (`decode_value`, `map_err`), which is why
//! the sharing stops where the bounds would start costing more than the
//! duplication does — binding parameters and the checked-write transaction
//! stay per backend.

use std::io::Write;

use futures_util::{Stream, TryStreamExt as _};
use sqlx::{Column as _, ColumnIndex, Database, Decode, Executor, IntoArguments, Row, Type};

use super::error::DbError;
use super::export::{export_io_err, ExportFormat, ExportSink};
use super::value::{cap_value, Capped, ColumnInfo, QueryResult, Value};

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
    Cancelled,
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
    let rest_cancelled = if truncated {
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
        rest_cancelled,
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
                if rest == Rest::Cancelled && e.code().as_deref() == Some(QUERY_CANCELED) =>
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
