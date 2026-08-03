//! Postgres backend: connecting and query execution. Introspection parity
//! (multi-schema, indexes, FKs) lands with FRE-11; until then only tables
//! and columns of the `public` schema are listed.

use std::io::Write;

use sqlx::postgres::types::{PgInterval, PgTimeTz};
use sqlx::postgres::{
    PgDatabaseError, PgErrorPosition, PgHasArrayType, PgPool, PgPoolOptions, PgRow, PgTypeKind,
    PgValueFormat, PgValueRef,
};
use sqlx::types::chrono::{DateTime, NaiveDate, NaiveDateTime, NaiveTime, Utc};
use sqlx::types::{Decimal, JsonValue, Uuid};
use sqlx::{Column as _, Row as _, TypeInfo as _, ValueRef as _};

use super::error::DbError;
use super::export::{export_io_err, ExportFormat, ExportSink};
use super::schema::{ColumnMeta, ForeignKeyMeta, Generated, IndexMeta, TableKind, TableMeta};
use super::staged::CheckedStatement;
use super::value::{cap_value, ColumnInfo, QueryResult, Value};

/// Splices a password into a Postgres URL (percent-encoding handled by the
/// url crate). Saved config stores URLs without passwords; this rebuilds the
/// full URL at connect time.
pub fn url_with_password(url: &str, password: &str) -> Result<String, DbError> {
    let mut parsed =
        url::Url::parse(url).map_err(|e| DbError::Connect(format!("invalid URL: {e}")))?;
    // set_password encodes most special characters but passes '%' through,
    // which would be mis-decoded on parse; encode it up front.
    let password = password.replace('%', "%25");
    parsed
        .set_password(Some(&password))
        .map_err(|_| DbError::Connect("this URL cannot carry a password".into()))?;
    Ok(parsed.into())
}

/// Canonicalizes a Postgres URL into the stable form used as a saved-connection
/// locator and keyring account key, so the same server written different ways
/// maps to one entry and one stored secret. Validates the scheme, then:
///
/// - strips any password (never persisted),
/// - rewrites `postgresql://` to `postgres://`,
/// - lowercases the host (DNS is case-insensitive; IP literals are unaffected),
/// - fills the default port `5432` when omitted, so `host` and `host:5432`
///   coincide.
///
/// Query params (e.g. `sslmode`) and the database path are left as-is.
pub fn normalize_pg_url(url: &str) -> Result<String, DbError> {
    let mut parsed =
        url::Url::parse(url.trim()).map_err(|e| DbError::Connect(format!("invalid URL: {e}")))?;
    if parsed.scheme() != "postgres" && parsed.scheme() != "postgresql" {
        return Err(DbError::Connect(format!(
            "expected a postgres:// URL, got {}://",
            parsed.scheme()
        )));
    }
    if parsed.scheme() == "postgresql" {
        // Both are non-special schemes, so this never fails; ignore defensively.
        let _ = parsed.set_scheme("postgres");
    }
    let _ = parsed.set_password(None);
    if let Some(host) = parsed.host_str() {
        let lowered = host.to_ascii_lowercase();
        if lowered != host {
            parsed
                .set_host(Some(&lowered))
                .map_err(|e| DbError::Connect(format!("invalid host: {e}")))?;
        }
    }
    match parsed.port() {
        // 0 is not a usable port; reject it here so a pasted URL is held to the
        // same rule as the connection form (FRE-42).
        Some(0) => return Err(DbError::Connect("port must be between 1 and 65535".into())),
        // postgres is a non-special scheme, so the url crate always serializes
        // an explicit port — the bare and `:5432` forms now serialize equal.
        None => {
            let _ = parsed.set_port(Some(5432));
        }
        Some(_) => {}
    }
    Ok(parsed.into())
}

/// The host and port a Postgres URL points at (default port 5432) — with an
/// SSH tunnel this is the address the SSH server must reach.
pub fn url_target(url: &str) -> Result<(String, u16), DbError> {
    let parsed = url::Url::parse(url).map_err(|e| DbError::Connect(format!("invalid URL: {e}")))?;
    let host = parsed
        .host_str()
        .ok_or_else(|| DbError::Connect("URL has no host".into()))?
        // IPv6 hosts come back bracketed; the forward target wants the bare
        // address.
        .trim_matches(['[', ']'])
        .to_string();
    Ok((host, parsed.port().unwrap_or(5432)))
}

/// Rewrites a URL to connect through a forwarded local port; everything else
/// (user, database, query params) is kept. The saved URL stays the logical
/// one — this form is only ever used for the actual connect.
pub fn url_via_local_port(url: &str, port: u16) -> Result<String, DbError> {
    let mut parsed =
        url::Url::parse(url).map_err(|e| DbError::Connect(format!("invalid URL: {e}")))?;
    parsed
        .set_host(Some("127.0.0.1"))
        .map_err(|e| DbError::Connect(format!("rewriting URL host: {e}")))?;
    parsed
        .set_port(Some(port))
        .map_err(|_| DbError::Connect("rewriting URL port failed".into()))?;
    Ok(parsed.into())
}

/// Builds a password-free URL from the individual connection-form fields.
pub fn build_url(
    host: &str,
    port: &str,
    database: &str,
    user: &str,
    sslmode: &str,
) -> Result<String, DbError> {
    let port = if port.trim().is_empty() {
        "5432".to_string()
    } else {
        port.trim().to_string()
    };
    if host.trim().is_empty() {
        return Err(DbError::Connect("host must not be empty".into()));
    }
    let mut parsed = url::Url::parse("postgres://localhost").expect("static base URL parses");
    parsed
        .set_host(Some(host.trim()))
        .map_err(|e| DbError::Connect(format!("invalid host: {e}")))?;
    let port_num: u16 = port
        .parse()
        .map_err(|_| DbError::Connect(format!("invalid port: {port}")))?;
    if port_num == 0 {
        return Err(DbError::Connect("port must be between 1 and 65535".into()));
    }
    parsed
        .set_port(Some(port_num))
        .map_err(|_| DbError::Connect("invalid port".into()))?;
    parsed
        .set_username(user.trim())
        .map_err(|_| DbError::Connect("invalid user".into()))?;
    // Only set a path for a non-empty database, so an empty db field converges
    // with a pasted URL that has no path (both → no trailing `/`).
    let database = database.trim();
    if !database.is_empty() {
        parsed.set_path(&format!("/{database}"));
    }
    if !sslmode.is_empty() {
        parsed.set_query(Some(&format!("sslmode={sslmode}")));
    }
    // Route through the normalizer so a form host typed as `MyHost` and a
    // pasted `myhost` URL land on the same canonical locator.
    normalize_pg_url(parsed.as_str())
}

/// Connects to Postgres from a URL (`postgres://user@host:port/db?sslmode=…`).
/// The URL may carry a password; saved config never does — callers splice a
/// session password in via [`url_with_password`].
pub async fn open_postgres(url: &str) -> Result<PgPool, DbError> {
    let pool = PgPoolOptions::new()
        .max_connections(4)
        .connect(url)
        .await
        .map_err(|e| DbError::Connect(friendly_connect_error(&e)))?;
    sqlx::query("SELECT 1")
        .fetch_one(&pool)
        .await
        .map_err(|e| DbError::Connect(friendly_connect_error(&e)))?;
    Ok(pool)
}

/// Categorizes common failure modes so the connections screen reads well:
/// auth, network/DNS, TLS, wrong-server.
fn friendly_connect_error(err: &sqlx::Error) -> String {
    let msg = err.to_string();
    let lower = msg.to_lowercase();
    if lower.contains("password authentication failed") {
        format!("authentication failed — {msg}")
    } else if lower.contains("role") && lower.contains("does not exist") {
        format!("unknown role — {msg}")
    } else if lower.contains("unexpected response from sslrequest") {
        // Something answered the Postgres handshake with garbage — usually a
        // different database server (e.g. SQL Server) on that host/port.
        format!("the server doesn't appear to be Postgres — check the host and port — {msg}")
    } else if lower.contains("tls") || lower.contains("ssl") {
        format!("TLS error — {msg}")
    } else if lower.contains("connection refused")
        || lower.contains("timed out")
        || lower.contains("failed to lookup")
    {
        format!("network error — {msg}")
    } else {
        msg
    }
}

type PgQuery<'q> = sqlx::query::Query<'q, sqlx::Postgres, sqlx::postgres::PgArguments>;

/// Binds backend-neutral [`Value`] parameters onto a prepared query.
///
/// NULL caveat: `Value::Null` binds as `None::<String>`, i.e. a NULL of
/// type text. That is fine for the current uses (filter values are text),
/// but Postgres rejects a text NULL for non-text columns in e.g.
/// `SET col = $n`. Cell editing solves this at the SQL level: the staged
/// SQL builders (`staged::ParamSql::value_sql`) render NULL values inline
/// as the literal `NULL` instead of binding them, so a NULL never reaches
/// this function from an edit.
fn bind_params<'q>(mut query: PgQuery<'q>, params: &[Value]) -> PgQuery<'q> {
    for param in params {
        query = match param {
            Value::Null => query.bind(None::<String>),
            Value::Integer(i) => query.bind(*i),
            Value::Real(r) => query.bind(*r),
            Value::Text(t) => query.bind(t.clone()),
            Value::Blob(b) => query.bind(b.clone()),
        };
    }
    query
}

pub async fn query_with(
    pool: &PgPool,
    sql: &str,
    params: &[Value],
) -> Result<QueryResult, DbError> {
    let rows = bind_params(sqlx::query(sql), params)
        .fetch_all(pool)
        .await
        .map_err(|e| query_error(e, sql))?;
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
/// bytes, so the free-form query path never scales with table or value size
/// (FRE-33). Returns the (bounded) result and whether more rows existed
/// beyond the cap. Shares the streaming primitive with [`export`].
pub async fn query_capped(
    pool: &PgPool,
    sql: &str,
    params: &[Value],
    max_rows: u64,
    cell_cap: usize,
) -> Result<(QueryResult, bool), DbError> {
    let stream = bind_params(sqlx::query(sql), params).fetch(pool);
    collect_capped(stream, sql, max_rows, cell_cap).await
}

/// [`query_capped`] against a single connection (e.g. one borrowed from a
/// transaction) rather than the pool — the read path for statements inside an
/// atomically-wrapped script (FRE-38). No bound params: scripts are raw text.
pub async fn query_capped_conn(
    conn: &mut sqlx::postgres::PgConnection,
    sql: &str,
    max_rows: u64,
    cell_cap: usize,
) -> Result<(QueryResult, bool), DbError> {
    let stream = sqlx::query(sql).fetch(&mut *conn);
    collect_capped(stream, sql, max_rows, cell_cap).await
}

/// Drains a row stream into a bounded [`QueryResult`], keeping at most
/// `max_rows` rows and capping each cell to `cell_cap` bytes; the bool is
/// whether rows existed past the cap. Shared by the pool and single-connection
/// capped readers.
async fn collect_capped<S>(
    mut stream: S,
    sql: &str,
    max_rows: u64,
    cell_cap: usize,
) -> Result<(QueryResult, bool), DbError>
where
    S: futures_util::Stream<Item = Result<PgRow, sqlx::Error>> + Unpin,
{
    use futures_util::TryStreamExt as _;

    let mut columns: Vec<ColumnInfo> = Vec::new();
    let mut out_rows: Vec<Vec<Value>> = Vec::new();
    let mut truncated = false;
    while let Some(row) = stream.try_next().await.map_err(|e| query_error(e, sql))? {
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
fn decode_row(row: &PgRow) -> Result<Vec<Value>, DbError> {
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
    pool: &PgPool,
    sql: &str,
    params: &[Value],
    format: ExportFormat,
    out: &mut impl Write,
) -> Result<u64, DbError> {
    use futures_util::TryStreamExt as _;

    let mut stream = bind_params(sqlx::query(sql), params).fetch(pool);
    let mut sink: Option<ExportSink> = None;
    let mut rows = 0u64;
    while let Some(row) = stream.try_next().await.map_err(|e| query_error(e, sql))? {
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
async fn describe_columns(pool: &PgPool, sql: &str) -> Result<Vec<String>, DbError> {
    use sqlx::Executor as _;
    let described = pool.describe(sql).await.map_err(|e| query_error(e, sql))?;
    Ok(described
        .columns()
        .iter()
        .map(|c| c.name().to_string())
        .collect())
}

/// Executes a statement without decoding rows, returning the driver's
/// affected-row count.
pub async fn execute(pool: &PgPool, sql: &str) -> Result<u64, DbError> {
    sqlx::query(sql)
        .execute(pool)
        .await
        .map(|done| done.rows_affected())
        .map_err(|e| query_error(e, sql))
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
    pool: &PgPool,
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
                return Err((Some(index), query_error(e, &statement.sql)));
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

/// Builds a query error, appending "line L, column C" when the server
/// reported an error cursor. Postgres sends the position as a 1-based
/// *character* index into the query text; positions into internally
/// generated queries ([`PgErrorPosition::Internal`]) don't map to the user's
/// text and are ignored.
fn query_error(err: sqlx::Error, sql: &str) -> DbError {
    let mut message = err.to_string();
    if let sqlx::Error::Database(db_err) = &err {
        if let Some(pg) = db_err.try_downcast_ref::<PgDatabaseError>() {
            if let Some(PgErrorPosition::Original(position)) = pg.position() {
                if let Some((line, column)) = line_col(sql, position) {
                    message.push_str(&format!(" (line {line}, column {column})"));
                }
            }
        }
    }
    DbError::Query(message)
}

/// Maps a 1-based character position into 1-based line and column numbers.
/// Returns `None` for positions outside the text (except one-past-the-end,
/// which the server reports e.g. for input that stops too early).
fn line_col(sql: &str, position: usize) -> Option<(usize, usize)> {
    if position == 0 {
        return None;
    }
    let mut line = 1usize;
    let mut column = 1usize;
    let mut seen = 0usize;
    for c in sql.chars() {
        seen += 1;
        if seen == position {
            return Some((line, column));
        }
        if c == '\n' {
            line += 1;
            column = 1;
        } else {
            column += 1;
        }
    }
    (position == seen + 1).then_some((line, column))
}

/// Full multi-schema introspection: every user schema's tables and views
/// with columns, primary keys, indexes (incl. unique), and foreign keys —
/// parity with the SQLite metadata model. Four batched queries regardless
/// of table count.
pub async fn introspect(pool: &PgPool) -> Result<Vec<TableMeta>, DbError> {
    let map_err = |e: sqlx::Error| DbError::Introspect(e.to_string());

    // Tables and views across all non-system schemas. Materialized views
    // (relkind 'm') are not in information_schema, so they come from a
    // pg_catalog UNION (FRE-41).
    let table_rows = sqlx::query(
        "SELECT table_schema, table_name, table_type \
         FROM information_schema.tables \
         WHERE table_schema NOT IN ('pg_catalog', 'information_schema') \
         UNION ALL \
         SELECT n.nspname, c.relname, 'MATERIALIZED VIEW' \
         FROM pg_class c \
         JOIN pg_namespace n ON n.oid = c.relnamespace \
         WHERE c.relkind = 'm' \
           AND n.nspname NOT IN ('pg_catalog', 'information_schema') \
         ORDER BY table_schema, table_name",
    )
    .fetch_all(pool)
    .await
    .map_err(map_err)?;

    // Columns with PK positions resolved in SQL (LEFT JOIN on the pkey
    // constraint), one row per column. Identity and generated columns have a
    // NULL column_default even though the database supplies their value, so
    // `is_identity`/`identity_generation`/`is_generated` are surfaced
    // separately and mapped into `ColumnMeta.generated` (FRE-25 required-
    // column detection and read-only gating).
    // Materialized-view columns aren't in information_schema.columns either, so
    // they come from pg_catalog (pg_attribute) via a UNION (FRE-41). Matviews
    // have no PK, identity, or generated columns, so those are constant here;
    // `format_type` yields the type name (with modifiers, e.g.
    // `character varying(255)`). `ord` orders columns within a relation across
    // both halves of the UNION.
    let column_rows = sqlx::query(
        "SELECT c.table_schema, c.table_name, c.column_name, c.data_type, \
                c.is_nullable, c.column_default, \
                c.is_identity, c.identity_generation, c.is_generated, \
                pk.ordinal_position AS pk_position, \
                c.ordinal_position AS ord \
         FROM information_schema.columns c \
         LEFT JOIN ( \
             SELECT kcu.table_schema, kcu.table_name, kcu.column_name, kcu.ordinal_position \
             FROM information_schema.table_constraints tc \
             JOIN information_schema.key_column_usage kcu \
               ON kcu.constraint_name = tc.constraint_name \
              AND kcu.constraint_schema = tc.constraint_schema \
              AND kcu.table_schema = tc.table_schema \
              AND kcu.table_name = tc.table_name \
             WHERE tc.constraint_type = 'PRIMARY KEY' \
         ) pk ON pk.table_schema = c.table_schema \
             AND pk.table_name = c.table_name \
             AND pk.column_name = c.column_name \
         WHERE c.table_schema NOT IN ('pg_catalog', 'information_schema') \
         UNION ALL \
         SELECT n.nspname, c.relname, a.attname, \
                format_type(a.atttypid, a.atttypmod), \
                CASE WHEN a.attnotnull THEN 'NO' ELSE 'YES' END, \
                NULL::text, 'NO', NULL::text, 'NEVER', \
                NULL::int, a.attnum::int \
         FROM pg_attribute a \
         JOIN pg_class c ON c.oid = a.attrelid \
         JOIN pg_namespace n ON n.oid = c.relnamespace \
         WHERE c.relkind = 'm' AND a.attnum > 0 AND NOT a.attisdropped \
           AND n.nspname NOT IN ('pg_catalog', 'information_schema') \
         ORDER BY table_schema, table_name, ord",
    )
    .fetch_all(pool)
    .await
    .map_err(map_err)?;

    // Indexes from pg_catalog (information_schema has no index view).
    // Expression-index entries have a 0 attnum and no attribute row; those
    // key positions surface as NULL column names. Partial indexes
    // (indpred) are flagged so row-identity detection can reject them;
    // invalid indexes (e.g. from a failed CREATE INDEX CONCURRENTLY) make
    // no guarantees at all and are dropped entirely.
    let index_rows = sqlx::query(
        "SELECT n.nspname AS table_schema, t.relname AS table_name, \
                i.relname AS index_name, ix.indisunique AS is_unique, \
                ix.indpred IS NOT NULL AS is_partial, \
                k.ord AS key_position, a.attname AS column_name \
         FROM pg_index ix \
         JOIN pg_class t ON t.oid = ix.indrelid \
         JOIN pg_class i ON i.oid = ix.indexrelid \
         JOIN pg_namespace n ON n.oid = t.relnamespace \
         CROSS JOIN LATERAL unnest(ix.indkey) WITH ORDINALITY AS k(attnum, ord) \
         LEFT JOIN pg_attribute a ON a.attrelid = t.oid AND a.attnum = k.attnum \
         WHERE n.nspname NOT IN ('pg_catalog', 'information_schema') \
           AND n.nspname NOT LIKE 'pg\\_%' \
           AND k.ord <= ix.indnkeyatts \
           AND ix.indisvalid \
         ORDER BY n.nspname, t.relname, i.relname, k.ord",
    )
    .fetch_all(pool)
    .await
    .map_err(map_err)?;

    // Foreign keys from pg_constraint; conkey/confkey arrays preserve the
    // multi-column ordering that information_schema loses.
    let fk_rows = sqlx::query(
        "SELECT n.nspname AS table_schema, t.relname AS table_name, \
                rn.nspname AS ref_schema, rt.relname AS ref_table, \
                (SELECT array_agg(a.attname ORDER BY x.ord) \
                 FROM unnest(c.conkey) WITH ORDINALITY AS x(attnum, ord) \
                 JOIN pg_attribute a ON a.attrelid = c.conrelid AND a.attnum = x.attnum \
                ) AS columns, \
                (SELECT array_agg(a.attname ORDER BY x.ord) \
                 FROM unnest(c.confkey) WITH ORDINALITY AS x(attnum, ord) \
                 JOIN pg_attribute a ON a.attrelid = c.confrelid AND a.attnum = x.attnum \
                ) AS ref_columns \
         FROM pg_constraint c \
         JOIN pg_class t ON t.oid = c.conrelid \
         JOIN pg_namespace n ON n.oid = t.relnamespace \
         JOIN pg_class rt ON rt.oid = c.confrelid \
         JOIN pg_namespace rn ON rn.oid = rt.relnamespace \
         WHERE c.contype = 'f' \
           AND n.nspname NOT IN ('pg_catalog', 'information_schema') \
           AND n.nspname NOT LIKE 'pg\\_%' \
         ORDER BY n.nspname, t.relname, c.conname",
    )
    .fetch_all(pool)
    .await
    .map_err(map_err)?;

    // Group the batched rows per (schema, table).
    let mut tables: Vec<TableMeta> = Vec::with_capacity(table_rows.len());
    for row in &table_rows {
        let table_type: String = get(row, "table_type")?;
        tables.push(TableMeta {
            schema: Some(get(row, "table_schema")?),
            name: get(row, "table_name")?,
            kind: match table_type.as_str() {
                "VIEW" => TableKind::View,
                "MATERIALIZED VIEW" => TableKind::MaterializedView,
                _ => TableKind::Table,
            },
            columns: Vec::new(),
            indexes: Vec::new(),
            foreign_keys: Vec::new(),
        });
    }
    let index_of = |schema: &str, name: &str, tables: &[TableMeta]| {
        tables
            .iter()
            .position(|t| t.schema.as_deref() == Some(schema) && t.name == name)
    };

    for row in &column_rows {
        let schema: String = get(row, "table_schema")?;
        let table: String = get(row, "table_name")?;
        let Some(idx) = index_of(&schema, &table, &tables) else {
            continue;
        };
        let nullable: String = get(row, "is_nullable")?;
        let pk_position: Option<i32> = get(row, "pk_position")?;
        // Both `GENERATED ALWAYS AS IDENTITY` and `GENERATED ALWAYS AS
        // (expr) STORED` are database-assigned and not writable, so both map
        // to `Generated::Always`; `GENERATED BY DEFAULT AS IDENTITY` is
        // overridable. `serial` is not generated — it carries a real
        // `nextval(…)` default and stays `Never`.
        let is_identity: String = get(row, "is_identity")?;
        let identity_generation: Option<String> = get(row, "identity_generation")?;
        let is_generated: String = get(row, "is_generated")?;
        let generated = if is_generated == "ALWAYS"
            || (is_identity == "YES" && identity_generation.as_deref() == Some("ALWAYS"))
        {
            Generated::Always
        } else if is_identity == "YES" {
            Generated::ByDefault
        } else {
            Generated::Never
        };
        tables[idx].columns.push(ColumnMeta {
            name: get(row, "column_name")?,
            type_name: get(row, "data_type")?,
            nullable: nullable == "YES",
            primary_key_position: pk_position.map(|p| p as u32),
            default: get::<Option<String>>(row, "column_default")?,
            generated,
        });
    }

    for row in &index_rows {
        let schema: String = get(row, "table_schema")?;
        let table: String = get(row, "table_name")?;
        let Some(idx) = index_of(&schema, &table, &tables) else {
            continue;
        };
        let index_name: String = get(row, "index_name")?;
        let column: Option<String> = get(row, "column_name")?;
        let column = column.unwrap_or_else(|| "<expr>".to_string());
        let unique: bool = get(row, "is_unique")?;
        let partial: bool = get(row, "is_partial")?;
        let indexes = &mut tables[idx].indexes;
        match indexes.last_mut() {
            Some(last) if last.name == index_name => last.columns.push(column),
            _ => indexes.push(IndexMeta {
                name: index_name,
                unique,
                partial,
                columns: vec![column],
            }),
        }
    }

    for row in &fk_rows {
        let schema: String = get(row, "table_schema")?;
        let table: String = get(row, "table_name")?;
        let Some(idx) = index_of(&schema, &table, &tables) else {
            continue;
        };
        let ref_columns: Vec<String> = get(row, "ref_columns")?;
        tables[idx].foreign_keys.push(ForeignKeyMeta {
            columns: get(row, "columns")?,
            referenced_schema: Some(get(row, "ref_schema")?),
            referenced_table: get(row, "ref_table")?,
            referenced_columns: ref_columns.into_iter().map(Some).collect(),
        });
    }

    Ok(tables)
}

fn get<'r, T: sqlx::Decode<'r, sqlx::Postgres> + sqlx::Type<sqlx::Postgres>>(
    row: &'r PgRow,
    column: &str,
) -> Result<T, DbError> {
    row.try_get(column)
        .map_err(|e| DbError::Introspect(format!("column {column}: {e}")))
}

/// Decodes scalar and rich Postgres types into the backend-neutral [`Value`]
/// model. Rich types (dates, numeric, uuid, json, intervals, arrays, enums)
/// render as `Value::Text` in a form close to what `psql` would print.
///
/// Cell data never errors the page: a type without a dedicated arm — or one
/// whose dedicated decode fails (NaN or >28-digit numeric, multidimensional
/// array, …) — degrades through [`decode_fallback`] to a text cast where
/// possible, then to a `<typename>` marker. The only `Err` left is an
/// out-of-range column index, which is a programming error.
fn decode_value(row: &PgRow, idx: usize) -> Result<Value, DbError> {
    let raw = row
        .try_get_raw(idx)
        .map_err(|e| DbError::Query(e.to_string()))?;
    if raw.is_null() {
        return Ok(Value::Null);
    }
    Ok(decode_typed(row, idx, &raw).unwrap_or_else(|| decode_fallback(row, idx, &raw)))
}

/// Type-specific decoding. Returns `None` both for types without a
/// dedicated arm and for values a dedicated arm cannot represent; the
/// caller degrades those via [`decode_fallback`].
fn decode_typed(row: &PgRow, idx: usize, raw: &PgValueRef) -> Option<Value> {
    match raw.type_info().name() {
        // Booleans render as text: "true"/"false" reads better in a viewer
        // than 0/1.
        "BOOL" => Some(Value::Text(row.try_get::<bool, _>(idx).ok()?.to_string())),
        "INT2" => Some(Value::Integer(row.try_get::<i16, _>(idx).ok()? as i64)),
        "INT4" => Some(Value::Integer(row.try_get::<i32, _>(idx).ok()? as i64)),
        "INT8" => Some(Value::Integer(row.try_get::<i64, _>(idx).ok()?)),
        "FLOAT4" => Some(Value::Real(row.try_get::<f32, _>(idx).ok()? as f64)),
        "FLOAT8" => Some(Value::Real(row.try_get::<f64, _>(idx).ok()?)),
        "TEXT" | "VARCHAR" | "BPCHAR" | "NAME" | "CHAR" => {
            Some(Value::Text(row.try_get::<String, _>(idx).ok()?))
        }
        "BYTEA" => Some(Value::Blob(row.try_get::<Vec<u8>, _>(idx).ok()?)),
        // Date/time family. `%.f` prints fractional seconds only when
        // non-zero; trailing zeros are trimmed to match Postgres output.
        // 'infinity'/'-infinity' must be caught from the wire bytes before
        // chrono: sqlx 0.8 panics (not errors) decoding them.
        "TIMESTAMP" => {
            if let Some(inf) = infinity_marker(raw) {
                return Some(Value::Text(inf.to_string()));
            }
            let ts = row.try_get::<NaiveDateTime, _>(idx).ok()?;
            Some(Value::Text(trim_fraction(
                ts.format("%Y-%m-%d %H:%M:%S%.f").to_string(),
            )))
        }
        "TIMESTAMPTZ" => {
            if let Some(inf) = infinity_marker(raw) {
                return Some(Value::Text(inf.to_string()));
            }
            // Postgres sends timestamptz as an instant; render in UTC with
            // an explicit offset so the timezone-awareness is visible.
            let ts = row.try_get::<DateTime<Utc>, _>(idx).ok()?;
            let local = trim_fraction(ts.format("%Y-%m-%d %H:%M:%S%.f").to_string());
            Some(Value::Text(format!("{local}+00:00")))
        }
        "DATE" => {
            if let Some(inf) = infinity_marker(raw) {
                return Some(Value::Text(inf.to_string()));
            }
            let d = row.try_get::<NaiveDate, _>(idx).ok()?;
            Some(Value::Text(d.format("%Y-%m-%d").to_string()))
        }
        "TIME" => {
            let t = row.try_get::<NaiveTime, _>(idx).ok()?;
            Some(Value::Text(trim_fraction(
                t.format("%H:%M:%S%.f").to_string(),
            )))
        }
        "TIMETZ" => {
            let t = row.try_get::<PgTimeTz, _>(idx).ok()?;
            let time = trim_fraction(t.time.format("%H:%M:%S%.f").to_string());
            Some(Value::Text(format!("{time}{}", t.offset)))
        }
        "INTERVAL" => {
            let iv = row.try_get::<PgInterval, _>(idx).ok()?;
            Some(Value::Text(format_interval(&iv)))
        }
        // Exact decimal string via rust_decimal — must not round-trip
        // through f64. NaN and >28 significant digits are not representable
        // and degrade to the marker.
        "NUMERIC" => Some(Value::Text(
            row.try_get::<Decimal, _>(idx).ok()?.to_string(),
        )),
        "UUID" => Some(Value::Text(row.try_get::<Uuid, _>(idx).ok()?.to_string())),
        // Compact JSON text (serde_json Display is compact).
        "JSON" | "JSONB" => Some(Value::Text(
            row.try_get::<JsonValue, _>(idx).ok()?.to_string(),
        )),
        // Arrays of the common element types render as a Postgres-style
        // literal. NULL elements render as NULL; text elements are not
        // quoted/escaped (this is a display form, not parseable syntax).
        // sqlx only decodes one-dimensional arrays; others degrade.
        "TEXT[]" | "VARCHAR[]" | "BPCHAR[]" | "NAME[]" => decode_array::<String>(row, idx, |v| v),
        "INT2[]" => decode_array::<i16>(row, idx, |v| v.to_string()),
        "INT4[]" => decode_array::<i32>(row, idx, |v| v.to_string()),
        "INT8[]" => decode_array::<i64>(row, idx, |v| v.to_string()),
        "FLOAT4[]" => decode_array::<f32>(row, idx, |v| v.to_string()),
        "FLOAT8[]" => decode_array::<f64>(row, idx, |v| v.to_string()),
        "BOOL[]" => decode_array::<bool>(row, idx, |v| v.to_string()),
        "UUID[]" => decode_array::<Uuid>(row, idx, |v| v.to_string()),
        "NUMERIC[]" => decode_array::<Decimal>(row, idx, |v| v.to_string()),
        _ => None,
    }
}

/// Graceful degradation for values [`decode_typed`] can't produce: enum
/// labels from the raw bytes, then a text cast, then a `<typename>` marker.
/// Infallible by design — one odd cell must not take down the page.
fn decode_fallback(row: &PgRow, idx: usize, raw: &PgValueRef) -> Value {
    // User-defined enums: the wire value (text or binary format) is the
    // label itself, but `try_get::<String>` refuses the unknown OID, so
    // read the raw bytes directly.
    if matches!(raw.type_info().kind(), PgTypeKind::Enum(_)) {
        if let Ok(label) = raw.as_str() {
            return Value::Text(label.to_string());
        }
    } else if let Ok(text) = row.try_get::<String, _>(idx) {
        return Value::Text(text);
    }
    Value::Text(format!("<{}>", raw.type_info().name().to_lowercase()))
}

/// Detects Postgres `infinity`/`-infinity` timestamp, timestamptz, and date
/// values from the wire bytes. Binary format encodes them as i64::MAX/MIN
/// (timestamp/timestamptz) or i32::MAX/MIN (date) big-endian; text format
/// spells them out. They must not reach chrono — sqlx 0.8 decodes via
/// `epoch + Duration`, whose overflow panics rather than erroring.
fn infinity_marker(raw: &PgValueRef) -> Option<&'static str> {
    let bytes = raw.as_bytes().ok()?;
    match raw.format() {
        PgValueFormat::Binary => {
            if bytes == i64::MAX.to_be_bytes() || bytes == i32::MAX.to_be_bytes() {
                Some("infinity")
            } else if bytes == i64::MIN.to_be_bytes() || bytes == i32::MIN.to_be_bytes() {
                Some("-infinity")
            } else if binary_datetime_exceeds_chrono(bytes) {
                // Postgres stores finite dates far beyond chrono's
                // ~year-262143 cap; sqlx's chrono decode would panic on the
                // overflow, so degrade those to a marker instead.
                Some("<out of chrono range>")
            } else {
                None
            }
        }
        // b"infinity" is coincidentally 8 bytes like a binary timestamp,
        // but the formats are branched on above so exact ASCII match here
        // is unambiguous.
        PgValueFormat::Text => match bytes {
            b"infinity" => Some("infinity"),
            b"-infinity" => Some("-infinity"),
            _ => None,
        },
    }
}

/// True when a binary timestamp (8-byte µs) or date (4-byte days) since the
/// 2000-01-01 Postgres epoch cannot be represented by chrono.
fn binary_datetime_exceeds_chrono(bytes: &[u8]) -> bool {
    let epoch_date = NaiveDate::from_ymd_opt(2000, 1, 1).expect("static date");
    match bytes.len() {
        8 => {
            let micros = i64::from_be_bytes(bytes.try_into().expect("length checked"));
            let epoch = epoch_date.and_hms_opt(0, 0, 0).expect("static time");
            // num_microseconds is None only if the span itself overflows
            // i64; treat that as "no bound" on the affected side.
            let max = NaiveDateTime::MAX
                .signed_duration_since(epoch)
                .num_microseconds()
                .unwrap_or(i64::MAX);
            let min = NaiveDateTime::MIN
                .signed_duration_since(epoch)
                .num_microseconds()
                .unwrap_or(i64::MIN);
            micros > max || micros < min
        }
        4 => {
            let days = i64::from(i32::from_be_bytes(
                bytes.try_into().expect("length checked"),
            ));
            let max = NaiveDate::MAX.signed_duration_since(epoch_date).num_days();
            let min = NaiveDate::MIN.signed_duration_since(epoch_date).num_days();
            days > max || days < min
        }
        _ => false,
    }
}

/// Decodes a one-dimensional array column and renders it as a
/// Postgres-style literal, e.g. `{a,b,NULL}`.
fn decode_array<T>(row: &PgRow, idx: usize, fmt: impl Fn(T) -> String) -> Option<Value>
where
    T: for<'a> sqlx::Decode<'a, sqlx::Postgres> + sqlx::Type<sqlx::Postgres> + PgHasArrayType,
{
    let items: Vec<Option<T>> = row.try_get(idx).ok()?;
    let mut parts = Vec::with_capacity(items.len());
    for item in items {
        parts.push(match item {
            Some(v) => fmt(v),
            None => "NULL".to_string(),
        });
    }
    Some(Value::Text(format!("{{{}}}", parts.join(","))))
}

/// Trims trailing zeros from a chrono-formatted fractional second: `%.f`
/// pads to 3/6/9 digits ("09.500"), Postgres prints minimal ("09.5"). The
/// input must end with the seconds field; the fraction dot is the only dot.
fn trim_fraction(mut s: String) -> String {
    if s.contains('.') {
        while s.ends_with('0') {
            s.pop();
        }
        if s.ends_with('.') {
            s.pop();
        }
    }
    s
}

/// Compact human form for an interval, e.g. "1 mon 2 days 03:04:05.5".
/// Zero parts are omitted; a zero interval renders as "00:00:00". Each
/// part carries its own sign, matching how Postgres prints mixed-sign
/// intervals.
fn format_interval(iv: &PgInterval) -> String {
    let mut parts: Vec<String> = Vec::new();
    if iv.months != 0 {
        let unit = if iv.months.abs() == 1 { "mon" } else { "mons" };
        parts.push(format!("{} {unit}", iv.months));
    }
    if iv.days != 0 {
        let unit = if iv.days.abs() == 1 { "day" } else { "days" };
        parts.push(format!("{} {unit}", iv.days));
    }
    if iv.microseconds != 0 || parts.is_empty() {
        let sign = if iv.microseconds < 0 { "-" } else { "" };
        let abs = iv.microseconds.unsigned_abs();
        let secs = abs / 1_000_000;
        let micros = abs % 1_000_000;
        let (h, m, s) = (secs / 3600, secs / 60 % 60, secs % 60);
        let mut time = format!("{sign}{h:02}:{m:02}:{s:02}");
        if micros != 0 {
            time.push_str(format!(".{micros:06}").trim_end_matches('0'));
        }
        parts.push(time);
    }
    parts.join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_with_password_splices_and_encodes() {
        let url =
            url_with_password("postgres://user@db.example.com:5432/app", "p@ss w%rd").unwrap();
        assert_eq!(
            url,
            "postgres://user:p%40ss%20w%25rd@db.example.com:5432/app"
        );
    }

    #[test]
    fn normalize_strips_password_and_checks_scheme() {
        assert_eq!(
            normalize_pg_url(" postgres://u:secret@h:5432/db?sslmode=require ").unwrap(),
            "postgres://u@h:5432/db?sslmode=require"
        );
        assert!(normalize_pg_url("mysql://u@h/db").is_err());
        assert!(normalize_pg_url("not a url").is_err());
    }

    #[test]
    fn normalize_canonicalizes_scheme_host_and_port() {
        // postgresql → postgres, default port filled, host lowercased.
        assert_eq!(
            normalize_pg_url("postgresql://user@Db.Example.COM/app").unwrap(),
            "postgres://user@db.example.com:5432/app"
        );
        // Already canonical: idempotent.
        let canonical = "postgres://user@db.example.com:5432/app";
        assert_eq!(normalize_pg_url(canonical).unwrap(), canonical);
    }

    #[test]
    fn build_url_rejects_a_zero_port() {
        // 0 parses as a valid u16 but is not a usable port (FRE-42).
        assert!(build_url("host", "0", "db", "user", "").is_err());
        assert!(build_url("host", "70000", "db", "user", "").is_err());
    }

    #[test]
    fn normalize_rejects_a_zero_port() {
        // The pasted-URL path is held to the same rule as the form fields.
        assert!(normalize_pg_url("postgres://user@host:0/db").is_err());
        assert!(normalize_pg_url("postgres://user@host:5432/db").is_ok());
    }

    #[test]
    fn form_and_paste_converge_for_an_empty_database() {
        // An empty database field must produce the same locator as a pasted URL
        // with no path — no phantom trailing-slash entry.
        let from_form = build_url("host", "", "", "user", "").unwrap();
        assert_eq!(from_form, "postgres://user@host:5432");
        assert_eq!(from_form, normalize_pg_url("postgres://user@host").unwrap());
    }

    #[test]
    fn equivalent_urls_normalize_to_the_same_locator() {
        // The same server written five ways must collapse to one locator, so a
        // saved list dedups and the keyring key matches.
        let forms = [
            "postgres://user@host:5432/db",
            "postgresql://user@host:5432/db",
            "postgres://user@host/db",
            "postgresql://user@HOST/db",
            "postgres://user:pw@host/db",
        ];
        let canonical = normalize_pg_url(forms[0]).unwrap();
        for form in forms {
            assert_eq!(normalize_pg_url(form).unwrap(), canonical, "{form}");
        }
    }

    #[test]
    fn build_url_assembles_fields_and_defaults_port() {
        assert_eq!(
            build_url("db.example.com", "", "app", "user", "prefer").unwrap(),
            "postgres://user@db.example.com:5432/app?sslmode=prefer"
        );
        assert_eq!(
            build_url(" h ", "6543", "d", "u", "require").unwrap(),
            "postgres://u@h:6543/d?sslmode=require"
        );
        assert!(build_url("h", "not-a-port", "d", "u", "prefer").is_err());
    }

    #[test]
    fn connect_errors_are_categorized() {
        let auth = sqlx::Error::Protocol("password authentication failed for user \"x\"".into());
        assert!(friendly_connect_error(&auth).starts_with("authentication failed"));
        let net = sqlx::Error::Protocol("Connection refused (os error 111)".into());
        assert!(friendly_connect_error(&net).starts_with("network error"));
        let tls = sqlx::Error::Protocol("TLS handshake failed".into());
        assert!(friendly_connect_error(&tls).starts_with("TLS error"));
        // A typo'd username is not a password problem — it must not trigger
        // the password prompt (which keys on "authentication failed").
        let role = sqlx::Error::Protocol("role \"nope\" does not exist".into());
        let friendly = friendly_connect_error(&role);
        assert!(friendly.starts_with("unknown role"));
        assert!(!friendly.contains("authentication failed"));
        // A non-Postgres server answering the handshake (e.g. SQL Server on
        // 1433) mentions SSL but is a wrong-server problem, not a TLS one.
        let not_pg = sqlx::Error::Protocol(
            "encountered unexpected or invalid data: unexpected response from SSLRequest: 0x00"
                .into(),
        );
        let friendly = friendly_connect_error(&not_pg);
        assert!(friendly.starts_with("the server doesn't appear to be Postgres"));
        assert!(!friendly.starts_with("TLS error"));
    }

    #[test]
    fn interval_formats_compactly() {
        let iv = |months, days, microseconds| PgInterval {
            months,
            days,
            microseconds,
        };
        // All parts present, plural units, fractional seconds trimmed.
        assert_eq!(
            format_interval(&iv(14, 2, (3 * 3600 + 4 * 60 + 5) * 1_000_000)),
            "14 mons 2 days 03:04:05"
        );
        assert_eq!(
            format_interval(&iv(1, 1, 5_000_000 + 500_000)),
            "1 mon 1 day 00:00:05.5"
        );
        // Zero parts are omitted; all-zero renders a zero time.
        assert_eq!(format_interval(&iv(0, 3, 0)), "3 days");
        assert_eq!(format_interval(&iv(0, 0, 0)), "00:00:00");
        // Negative components each carry their sign.
        assert_eq!(
            format_interval(&iv(-1, -2, -5_000_000)),
            "-1 mon -2 days -00:00:05"
        );
    }

    #[test]
    fn url_target_extracts_host_and_defaults_port() {
        assert_eq!(
            url_target("postgres://u@db.internal/app").unwrap(),
            ("db.internal".to_string(), 5432)
        );
        assert_eq!(
            url_target("postgres://u@db.internal:6543/app").unwrap(),
            ("db.internal".to_string(), 6543)
        );
        assert_eq!(
            url_target("postgres://u@[::1]:6543/app").unwrap(),
            ("::1".to_string(), 6543)
        );
        assert!(url_target("not a url").is_err());
    }

    #[test]
    fn url_via_local_port_rewrites_only_host_and_port() {
        assert_eq!(
            url_via_local_port("postgres://u@db.internal:5432/app?sslmode=disable", 40123).unwrap(),
            "postgres://u@127.0.0.1:40123/app?sslmode=disable"
        );
    }

    #[test]
    fn line_col_maps_character_positions() {
        let sql = "SELECT 1 +\n  bad_col\nFROM t";
        assert_eq!(line_col(sql, 1), Some((1, 1)));
        assert_eq!(line_col(sql, 8), Some((1, 8)));
        // First char of line 2 ("SELECT 1 +\n" is 11 chars).
        assert_eq!(line_col(sql, 12), Some((2, 1)));
        assert_eq!(line_col(sql, 14), Some((2, 3)));
        // One past the end is valid (server points at missing input).
        let len = sql.chars().count();
        assert_eq!(line_col(sql, len + 1), Some((3, 7)));
        // Zero and far-out-of-range positions are dropped.
        assert_eq!(line_col(sql, 0), None);
        assert_eq!(line_col(sql, len + 2), None);
    }

    #[test]
    fn line_col_counts_characters_not_bytes() {
        // "ыыы" is 6 bytes but 3 chars; position 5 is the 'X'.
        assert_eq!(line_col("ыыыаX", 5), Some((1, 5)));
    }

    #[test]
    fn build_url_rejects_an_empty_host() {
        let err = build_url("  ", "5432", "db", "u", "prefer").unwrap_err();
        assert!(err.to_string().contains("host"));
    }
}
