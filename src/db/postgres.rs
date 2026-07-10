//! Postgres backend: connecting and query execution. Introspection parity
//! (multi-schema, indexes, FKs) lands with FRE-11; until then only tables
//! and columns of the `public` schema are listed.

use sqlx::postgres::types::{PgInterval, PgTimeTz};
use sqlx::postgres::{PgHasArrayType, PgPool, PgPoolOptions, PgRow, PgTypeKind};
use sqlx::types::chrono::{DateTime, NaiveDate, NaiveDateTime, NaiveTime, Utc};
use sqlx::types::{Decimal, JsonValue, Uuid};
use sqlx::{Column as _, Row as _, TypeInfo as _, ValueRef as _};

use super::error::DbError;
use super::schema::{ColumnMeta, ForeignKeyMeta, IndexMeta, TableKind, TableMeta};
use super::value::{ColumnInfo, QueryResult, Value};

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

/// Strips any password so the URL is safe to persist; also validates the
/// scheme.
pub fn sanitized_url(url: &str) -> Result<String, DbError> {
    let mut parsed =
        url::Url::parse(url.trim()).map_err(|e| DbError::Connect(format!("invalid URL: {e}")))?;
    if parsed.scheme() != "postgres" && parsed.scheme() != "postgresql" {
        return Err(DbError::Connect(format!(
            "expected a postgres:// URL, got {}://",
            parsed.scheme()
        )));
    }
    let _ = parsed.set_password(None);
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
    parsed
        .set_port(Some(
            port.parse()
                .map_err(|_| DbError::Connect(format!("invalid port: {port}")))?,
        ))
        .map_err(|_| DbError::Connect("invalid port".into()))?;
    parsed
        .set_username(user.trim())
        .map_err(|_| DbError::Connect("invalid user".into()))?;
    parsed.set_path(&format!("/{}", database.trim()));
    if !sslmode.is_empty() {
        parsed.set_query(Some(&format!("sslmode={sslmode}")));
    }
    Ok(parsed.into())
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
/// auth, network/DNS, TLS.
fn friendly_connect_error(err: &sqlx::Error) -> String {
    let msg = err.to_string();
    let lower = msg.to_lowercase();
    if lower.contains("password authentication failed") {
        format!("authentication failed — {msg}")
    } else if lower.contains("role") && lower.contains("does not exist") {
        format!("unknown role — {msg}")
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

pub async fn query_with(
    pool: &PgPool,
    sql: &str,
    params: &[Value],
) -> Result<QueryResult, DbError> {
    let mut prepared = sqlx::query(sql);
    for param in params {
        prepared = match param {
            Value::Null => prepared.bind(None::<String>),
            Value::Integer(i) => prepared.bind(*i),
            Value::Real(r) => prepared.bind(*r),
            Value::Text(t) => prepared.bind(t.clone()),
            Value::Blob(b) => prepared.bind(b.clone()),
        };
    }
    let rows = prepared
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
        let mut values = Vec::with_capacity(row.columns().len());
        for idx in 0..row.columns().len() {
            values.push(decode_value(row, idx)?);
        }
        out_rows.push(values);
    }
    Ok(QueryResult {
        columns,
        rows: out_rows,
    })
}

/// Full multi-schema introspection: every user schema's tables and views
/// with columns, primary keys, indexes (incl. unique), and foreign keys —
/// parity with the SQLite metadata model. Four batched queries regardless
/// of table count.
pub async fn introspect(pool: &PgPool) -> Result<Vec<TableMeta>, DbError> {
    let map_err = |e: sqlx::Error| DbError::Introspect(e.to_string());

    // Tables and views across all non-system schemas.
    let table_rows = sqlx::query(
        "SELECT table_schema, table_name, table_type \
         FROM information_schema.tables \
         WHERE table_schema NOT IN ('pg_catalog', 'information_schema') \
         ORDER BY table_schema, table_name",
    )
    .fetch_all(pool)
    .await
    .map_err(map_err)?;

    // Columns with PK positions resolved in SQL (LEFT JOIN on the pkey
    // constraint), one row per column.
    let column_rows = sqlx::query(
        "SELECT c.table_schema, c.table_name, c.column_name, c.data_type, \
                c.is_nullable, c.column_default, pk.ordinal_position AS pk_position \
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
         ORDER BY c.table_schema, c.table_name, c.ordinal_position",
    )
    .fetch_all(pool)
    .await
    .map_err(map_err)?;

    // Indexes from pg_catalog (information_schema has no index view).
    // Expression-index entries have a 0 attnum and no attribute row; those
    // key positions surface as NULL column names.
    let index_rows = sqlx::query(
        "SELECT n.nspname AS table_schema, t.relname AS table_name, \
                i.relname AS index_name, ix.indisunique AS is_unique, \
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
            kind: if table_type == "VIEW" {
                TableKind::View
            } else {
                TableKind::Table
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
        tables[idx].columns.push(ColumnMeta {
            name: get(row, "column_name")?,
            type_name: get(row, "data_type")?,
            nullable: nullable == "YES",
            primary_key_position: pk_position.map(|p| p as u32),
            default: get::<Option<String>>(row, "column_default")?,
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
        let indexes = &mut tables[idx].indexes;
        match indexes.last_mut() {
            Some(last) if last.name == index_name => last.columns.push(column),
            _ => indexes.push(IndexMeta {
                name: index_name,
                unique,
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
/// render as `Value::Text` in a form close to what `psql` would print;
/// anything unhandled falls back to a text cast where possible, then to a
/// `<typename>` marker — never an error for the whole page.
fn decode_value(row: &PgRow, idx: usize) -> Result<Value, DbError> {
    let raw = row
        .try_get_raw(idx)
        .map_err(|e| DbError::Query(e.to_string()))?;
    if raw.is_null() {
        return Ok(Value::Null);
    }
    let type_name = raw.type_info().name().to_string();
    let map_err = |e: sqlx::Error| DbError::Query(e.to_string());
    let decoded = match type_name.as_str() {
        // Booleans render as text: "true"/"false" reads better in a viewer
        // than 0/1.
        "BOOL" => Value::Text(row.try_get::<bool, _>(idx).map_err(map_err)?.to_string()),
        "INT2" => Value::Integer(row.try_get::<i16, _>(idx).map_err(map_err)? as i64),
        "INT4" => Value::Integer(row.try_get::<i32, _>(idx).map_err(map_err)? as i64),
        "INT8" => Value::Integer(row.try_get::<i64, _>(idx).map_err(map_err)?),
        "FLOAT4" => Value::Real(row.try_get::<f32, _>(idx).map_err(map_err)? as f64),
        "FLOAT8" => Value::Real(row.try_get::<f64, _>(idx).map_err(map_err)?),
        "TEXT" | "VARCHAR" | "BPCHAR" | "NAME" | "CHAR" => {
            Value::Text(row.try_get::<String, _>(idx).map_err(map_err)?)
        }
        "BYTEA" => Value::Blob(row.try_get::<Vec<u8>, _>(idx).map_err(map_err)?),
        // Date/time family. `%.f` prints fractional seconds only when
        // non-zero, with no trailing zeros — matching Postgres output.
        "TIMESTAMP" => {
            let ts = row.try_get::<NaiveDateTime, _>(idx).map_err(map_err)?;
            Value::Text(trim_fraction(ts.format("%Y-%m-%d %H:%M:%S%.f").to_string()))
        }
        "TIMESTAMPTZ" => {
            // Postgres sends timestamptz as an instant; render in UTC with
            // an explicit offset so the timezone-awareness is visible.
            let ts = row.try_get::<DateTime<Utc>, _>(idx).map_err(map_err)?;
            let local = trim_fraction(ts.format("%Y-%m-%d %H:%M:%S%.f").to_string());
            Value::Text(format!("{local}+00:00"))
        }
        "DATE" => {
            let d = row.try_get::<NaiveDate, _>(idx).map_err(map_err)?;
            Value::Text(d.format("%Y-%m-%d").to_string())
        }
        "TIME" => {
            let t = row.try_get::<NaiveTime, _>(idx).map_err(map_err)?;
            Value::Text(trim_fraction(t.format("%H:%M:%S%.f").to_string()))
        }
        "TIMETZ" => {
            let t = row.try_get::<PgTimeTz, _>(idx).map_err(map_err)?;
            let time = trim_fraction(t.time.format("%H:%M:%S%.f").to_string());
            Value::Text(format!("{time}{}", t.offset))
        }
        "INTERVAL" => {
            let iv = row.try_get::<PgInterval, _>(idx).map_err(map_err)?;
            Value::Text(format_interval(&iv))
        }
        // Exact decimal string via rust_decimal — must not round-trip
        // through f64.
        "NUMERIC" => Value::Text(row.try_get::<Decimal, _>(idx).map_err(map_err)?.to_string()),
        "UUID" => Value::Text(row.try_get::<Uuid, _>(idx).map_err(map_err)?.to_string()),
        // Compact JSON text (serde_json Display is compact).
        "JSON" | "JSONB" => Value::Text(
            row.try_get::<JsonValue, _>(idx)
                .map_err(map_err)?
                .to_string(),
        ),
        // Arrays of the common element types render as a Postgres-style
        // literal. NULL elements render as NULL; text elements are not
        // quoted/escaped (this is a display form, not parseable syntax).
        "TEXT[]" | "VARCHAR[]" | "BPCHAR[]" | "NAME[]" => {
            decode_array::<String>(row, idx, |v| v).map_err(map_err)?
        }
        "INT2[]" => decode_array::<i16>(row, idx, |v| v.to_string()).map_err(map_err)?,
        "INT4[]" => decode_array::<i32>(row, idx, |v| v.to_string()).map_err(map_err)?,
        "INT8[]" => decode_array::<i64>(row, idx, |v| v.to_string()).map_err(map_err)?,
        "FLOAT4[]" => decode_array::<f32>(row, idx, |v| v.to_string()).map_err(map_err)?,
        "FLOAT8[]" => decode_array::<f64>(row, idx, |v| v.to_string()).map_err(map_err)?,
        "BOOL[]" => decode_array::<bool>(row, idx, |v| v.to_string()).map_err(map_err)?,
        "UUID[]" => decode_array::<Uuid>(row, idx, |v| v.to_string()).map_err(map_err)?,
        "NUMERIC[]" => decode_array::<Decimal>(row, idx, |v| v.to_string()).map_err(map_err)?,
        _ => {
            // User-defined enums: the wire value (text or binary format) is
            // the label itself, but `try_get::<String>` refuses the unknown
            // OID, so read the raw bytes directly.
            if matches!(raw.type_info().kind(), PgTypeKind::Enum(_)) {
                match raw.as_str() {
                    Ok(label) => Value::Text(label.to_string()),
                    Err(_) => Value::Text(format!("<{}>", type_name.to_lowercase())),
                }
            } else {
                match row.try_get::<String, _>(idx) {
                    Ok(text) => Value::Text(text),
                    // Unknown type that won't decode as text (e.g. ranges,
                    // inet, money); show a marker rather than erroring the
                    // whole page.
                    Err(_) => Value::Text(format!("<{}>", type_name.to_lowercase())),
                }
            }
        }
    };
    Ok(decoded)
}

/// Decodes a one-dimensional array column and renders it as a
/// Postgres-style literal, e.g. `{a,b,NULL}`.
fn decode_array<T>(row: &PgRow, idx: usize, fmt: impl Fn(T) -> String) -> Result<Value, sqlx::Error>
where
    T: for<'a> sqlx::Decode<'a, sqlx::Postgres> + sqlx::Type<sqlx::Postgres> + PgHasArrayType,
{
    let items: Vec<Option<T>> = row.try_get(idx)?;
    let mut parts = Vec::with_capacity(items.len());
    for item in items {
        parts.push(match item {
            Some(v) => fmt(v),
            None => "NULL".to_string(),
        });
    }
    Ok(Value::Text(format!("{{{}}}", parts.join(","))))
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
    fn sanitized_url_strips_password_and_checks_scheme() {
        assert_eq!(
            sanitized_url(" postgres://u:secret@h:5432/db?sslmode=require ").unwrap(),
            "postgres://u@h:5432/db?sslmode=require"
        );
        assert!(sanitized_url("mysql://u@h/db").is_err());
        assert!(sanitized_url("not a url").is_err());
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
    fn build_url_rejects_an_empty_host() {
        let err = build_url("  ", "5432", "db", "u", "prefer").unwrap_err();
        assert!(err.to_string().contains("host"));
    }
}
