//! SQLite backend: connecting, introspection, and query execution.

use std::io::Write;
use std::path::Path;

use sqlx::sqlite::{SqliteConnectOptions, SqlitePool, SqlitePoolOptions, SqliteRow};
use sqlx::{Column as _, Row as _, TypeInfo as _, ValueRef as _};

use super::error::DbError;
use super::export::{export_io_err, ExportFormat, ExportSink};
use super::page::quote_ident;
use super::schema::{
    ColumnMeta, ForeignKeyMeta, Generated, IndexMeta, TableKind, TableMeta, TypeDetail,
};
use super::staged::CheckedStatement;
use super::value::{cap_value, ColumnInfo, QueryResult, Value};

/// Opens an existing SQLite database file and validates it is actually a
/// SQLite database (the file header is only checked on first real access).
pub async fn open_sqlite(path: &Path) -> Result<SqlitePool, DbError> {
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
/// [`Value`] model.
pub async fn query(pool: &SqlitePool, sql: &str) -> Result<QueryResult, DbError> {
    query_with(pool, sql, &[]).await
}

/// Executes a statement without decoding rows, returning the driver's
/// affected-row count. sqlx 0.8 does not expose `sqlite3_error_offset` (its
/// `SqliteError` carries only code + message), so failures have no position
/// info beyond the `near "…"` context SQLite puts in the message itself.
pub async fn execute(pool: &SqlitePool, sql: &str) -> Result<u64, DbError> {
    sqlx::query(sql)
        .execute(pool)
        .await
        .map(|done| done.rows_affected())
        .map_err(|e| DbError::Query(e.to_string()))
}

/// Executes parameterized writes inside ONE transaction, committing only
/// when every statement affected exactly its `expected_rows` rows. Any SQL
/// error or count mismatch rolls the whole batch back; the error carries the
/// index of the failing statement (`None` for begin/commit failures, which
/// belong to no statement). This is the safety net for row edits: a WHERE
/// clause that unexpectedly matches more (or fewer) rows than the one being
/// edited must never commit — and with staged edits (FRE-14), neither may
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
                return Err((Some(index), DbError::Query(e.to_string())));
            }
        };
        let affected = done.rows_affected();
        if affected != statement.expected_rows {
            let _ = tx.rollback().await;
            return Err((
                Some(index),
                DbError::RowCountMismatch(format!(
                    "statement affected {affected} rows, expected {} — rolled back",
                    statement.expected_rows
                )),
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
pub async fn query_with(
    pool: &SqlitePool,
    sql: &str,
    params: &[Value],
) -> Result<QueryResult, DbError> {
    let rows = bind_params(sqlx::query(sql), params)
        .fetch_all(pool)
        .await
        .map_err(|e| DbError::Query(e.to_string()))?;
    let columns = match rows.first() {
        Some(row) => row
            .columns()
            .iter()
            .map(|c| ColumnInfo {
                name: c.name().to_string(),
            })
            .collect(),
        None => Vec::new(),
    };
    let mut out_rows = Vec::with_capacity(rows.len());
    for row in &rows {
        out_rows.push(decode_row(row)?);
    }
    Ok(QueryResult {
        columns,
        rows: out_rows,
    })
}

/// Streams `sql` one row at a time (`fetch`, not `fetch_all`), decoding and
/// retaining at most `max_rows` rows and capping each cell to `cell_cap`
/// bytes, so neither the row count nor a pathologically large cell can make
/// the free-form query path scale with table or value size (FRE-33). Returns
/// the (bounded) result and whether more rows existed beyond the cap. Shares
/// the same streaming primitive as [`export`].
pub async fn query_capped(
    pool: &SqlitePool,
    sql: &str,
    params: &[Value],
    max_rows: u64,
    cell_cap: usize,
) -> Result<(QueryResult, bool), DbError> {
    let stream = bind_params(sqlx::query(sql), params).fetch(pool);
    collect_capped(stream, max_rows, cell_cap).await
}

/// [`query_capped`] against a single connection (e.g. one borrowed from a
/// transaction) rather than the pool — the read path for statements inside an
/// atomically-wrapped script (FRE-38). No bound params: scripts are raw text.
pub async fn query_capped_conn(
    conn: &mut sqlx::sqlite::SqliteConnection,
    sql: &str,
    max_rows: u64,
    cell_cap: usize,
) -> Result<(QueryResult, bool), DbError> {
    let stream = sqlx::query(sql).fetch(&mut *conn);
    collect_capped(stream, max_rows, cell_cap).await
}

/// Runs a non-row statement on a single connection (e.g. one borrowed from a
/// transaction) rather than the pool, returning affected rows — the write
/// path for statements inside an atomically-wrapped script (FRE-38).
pub async fn execute_conn(
    conn: &mut sqlx::sqlite::SqliteConnection,
    sql: &str,
) -> Result<u64, DbError> {
    sqlx::query(sql)
        .execute(conn)
        .await
        .map(|done| done.rows_affected())
        .map_err(|e| DbError::Query(e.to_string()))
}

/// Commits a script transaction — its statements all take effect.
pub async fn commit_tx(tx: sqlx::Transaction<'_, sqlx::Sqlite>) -> Result<(), DbError> {
    tx.commit().await.map_err(|e| DbError::Query(e.to_string()))
}

/// Rolls a script transaction back. Best-effort: a rollback failure leaves
/// nothing committed anyway (the transaction also rolls back on drop).
pub async fn rollback_tx(tx: sqlx::Transaction<'_, sqlx::Sqlite>) {
    let _ = tx.rollback().await;
}

/// Drains a row stream into a bounded [`QueryResult`], keeping at most
/// `max_rows` rows and capping each cell to `cell_cap` bytes; the bool is
/// whether rows existed past the cap. Shared by the pool and single-connection
/// capped readers.
async fn collect_capped<S>(
    mut stream: S,
    max_rows: u64,
    cell_cap: usize,
) -> Result<(QueryResult, bool), DbError>
where
    S: futures_util::Stream<Item = Result<SqliteRow, sqlx::Error>> + Unpin,
{
    use futures_util::TryStreamExt as _;

    let mut columns: Vec<ColumnInfo> = Vec::new();
    let mut out_rows: Vec<Vec<Value>> = Vec::new();
    let mut truncated = false;
    while let Some(row) = stream
        .try_next()
        .await
        .map_err(|e| DbError::Query(e.to_string()))?
    {
        // The cap+1'th row that reaches us proves there is more; stop before
        // decoding it so exactly `max_rows` rows are retained.
        if out_rows.len() as u64 >= max_rows {
            truncated = true;
            break;
        }
        if columns.is_empty() {
            columns = row
                .columns()
                .iter()
                .map(|c| ColumnInfo {
                    name: c.name().to_string(),
                })
                .collect();
        }
        let values = decode_row(&row)?
            .into_iter()
            .map(|v| cap_value(v, cell_cap))
            .collect();
        out_rows.push(values);
    }
    Ok((
        QueryResult {
            columns,
            rows: out_rows,
        },
        truncated,
    ))
}

/// Decodes every cell of one fetched row into the backend-neutral [`Value`]
/// model. Shared by the buffered ([`query_with`]) and streaming
/// ([`query_capped`], [`export`]) paths.
fn decode_row(row: &SqliteRow) -> Result<Vec<Value>, DbError> {
    let mut values = Vec::with_capacity(row.columns().len());
    for idx in 0..row.columns().len() {
        values.push(decode_value(row, idx)?);
    }
    Ok(values)
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
    use futures_util::TryStreamExt as _;

    let mut stream = bind_params(sqlx::query(sql), params).fetch(pool);
    let mut sink: Option<ExportSink> = None;
    let mut rows = 0u64;
    while let Some(row) = stream
        .try_next()
        .await
        .map_err(|e| DbError::Query(e.to_string()))?
    {
        let sink = match sink.as_mut() {
            Some(sink) => sink,
            None => {
                let columns = row.columns().iter().map(|c| c.name().to_string()).collect();
                let mut new_sink = ExportSink::new(format, columns);
                new_sink.begin(out).map_err(export_io_err)?;
                sink.insert(new_sink)
            }
        };
        let values = decode_row(&row)?;
        sink.write_row(&values, out).map_err(export_io_err)?;
        rows += 1;
    }
    match sink.as_mut() {
        Some(sink) => sink.end(out).map_err(export_io_err)?,
        // No rows streamed: describe the statement so an empty export still
        // carries the column header (CSV) or a well-formed empty array (JSON).
        None => {
            let columns = describe_columns(pool, sql).await?;
            let mut sink = ExportSink::new(format, columns);
            sink.begin(out).map_err(export_io_err)?;
            sink.end(out).map_err(export_io_err)?;
        }
    }
    Ok(rows)
}

/// Column names of a prepared statement, for the header of a zero-row export.
async fn describe_columns(pool: &SqlitePool, sql: &str) -> Result<Vec<String>, DbError> {
    use sqlx::Executor as _;
    let described = pool
        .describe(sql)
        .await
        .map_err(|e| DbError::Query(e.to_string()))?;
    Ok(described
        .columns()
        .iter()
        .map(|c| c.name().to_string())
        .collect())
}

/// Lists tables and views with columns, indexes, and foreign keys.
pub async fn introspect(pool: &SqlitePool) -> Result<Vec<TableMeta>, DbError> {
    let entries = sqlx::query(
        "SELECT name, type FROM sqlite_master \
         WHERE type IN ('table', 'view') AND name NOT LIKE 'sqlite_%' \
         ORDER BY name",
    )
    .fetch_all(pool)
    .await
    .map_err(|e| DbError::Introspect(e.to_string()))?;

    let mut tables = Vec::with_capacity(entries.len());
    for entry in entries {
        let name: String = get(&entry, "name")?;
        let kind: String = get(&entry, "type")?;
        let kind = if kind == "view" {
            TableKind::View
        } else {
            TableKind::Table
        };
        tables.push(TableMeta {
            schema: None,
            columns: table_columns(pool, &name).await?,
            indexes: table_indexes(pool, &name).await?,
            foreign_keys: table_foreign_keys(pool, &name).await?,
            name,
            kind,
            // No per-object narrowing: kind and row identity already carry
            // everything this backend knows about writability (FRE-87).
            restriction: None,
        });
    }
    Ok(tables)
}

async fn table_columns(pool: &SqlitePool, table: &str) -> Result<Vec<ColumnMeta>, DbError> {
    // `table_xinfo` (not `table_info`) so generated (`AS (…)`) columns are
    // reported: they appear in `SELECT *`, so the grid's bounded page fetch —
    // which builds an explicit projection from this metadata (FRE-33) — must
    // know about them, and they must be read-only. `table_xinfo` adds a
    // `hidden` column: 0 = ordinary, 2 = VIRTUAL generated, 3 = STORED
    // generated, 1 = a hidden column (virtual-table machinery) that `SELECT *`
    // does NOT return — skip those so the projection still matches `SELECT *`.
    let rows = pragma(pool, "table_xinfo", table).await?;
    let mut columns = Vec::with_capacity(rows.len());
    for row in rows {
        let hidden: i64 = get(&row, "hidden")?;
        if hidden == 1 {
            continue;
        }
        let pk: i64 = get(&row, "pk")?;
        let notnull: i64 = get(&row, "notnull")?;
        columns.push(ColumnMeta {
            name: get(&row, "name")?,
            type_name: get(&row, "type")?,
            nullable: notnull == 0,
            primary_key_position: (pk > 0).then_some(pk as u32),
            default: get::<Option<String>>(&row, "dflt_value")?,
            // 2/3 are VIRTUAL/STORED generated columns: database-assigned and
            // not writable through ordinary INSERT/UPDATE.
            generated: if hidden == 2 || hidden == 3 {
                Generated::Always
            } else {
                Generated::Never
            },
            // SQLite has no enum or array types.
            type_detail: TypeDetail::Plain,
        });
    }
    Ok(columns)
}

async fn table_indexes(pool: &SqlitePool, table: &str) -> Result<Vec<IndexMeta>, DbError> {
    let rows = pragma(pool, "index_list", table).await?;
    let mut indexes = Vec::with_capacity(rows.len());
    for row in rows {
        let name: String = get(&row, "name")?;
        let unique: i64 = get(&row, "unique")?;
        let partial: i64 = get(&row, "partial")?;
        let column_rows = pragma(pool, "index_info", &name).await?;
        let mut columns: Vec<(i64, Option<String>)> = Vec::with_capacity(column_rows.len());
        for col in column_rows {
            columns.push((get(&col, "seqno")?, get(&col, "name")?));
        }
        columns.sort_by_key(|(seqno, _)| *seqno);
        indexes.push(IndexMeta {
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

async fn table_foreign_keys(
    pool: &SqlitePool,
    table: &str,
) -> Result<Vec<ForeignKeyMeta>, DbError> {
    let rows = pragma(pool, "foreign_key_list", table).await?;
    // Rows arrive one per column, keyed by (id, seq); group them into FKs.
    let mut parts: Vec<(i64, i64, String, String, Option<String>)> = Vec::new();
    for row in rows {
        parts.push((
            get(&row, "id")?,
            get(&row, "seq")?,
            get(&row, "table")?,
            get(&row, "from")?,
            get(&row, "to")?,
        ));
    }
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
    Ok(fks.into_iter().map(|(_, fk)| fk).collect())
}

async fn pragma(pool: &SqlitePool, pragma: &str, arg: &str) -> Result<Vec<SqliteRow>, DbError> {
    let sql = format!("PRAGMA {pragma}({})", quote_ident(arg));
    sqlx::query(&sql)
        .fetch_all(pool)
        .await
        .map_err(|e| DbError::Introspect(e.to_string()))
}

fn get<'r, T: sqlx::Decode<'r, sqlx::Sqlite> + sqlx::Type<sqlx::Sqlite>>(
    row: &'r SqliteRow,
    column: &str,
) -> Result<T, DbError> {
    row.try_get(column)
        .map_err(|e| DbError::Introspect(format!("column {column}: {e}")))
}

fn decode_value(row: &SqliteRow, idx: usize) -> Result<Value, DbError> {
    let raw = row
        .try_get_raw(idx)
        .map_err(|e| DbError::Query(e.to_string()))?;
    if raw.is_null() {
        return Ok(Value::Null);
    }
    let type_name = raw.type_info().name().to_string();
    let decoded = match type_name.as_str() {
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
    use super::*;

    #[test]
    fn quote_ident_escapes_embedded_quotes() {
        assert_eq!(quote_ident("plain"), "\"plain\"");
        assert_eq!(quote_ident("we\"ird"), "\"we\"\"ird\"");
        assert_eq!(quote_ident("with space"), "\"with space\"");
    }
}
