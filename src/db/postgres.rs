//! Postgres backend: connecting and query execution. Introspection parity
//! (multi-schema, indexes, FKs) lands with FRE-11; until then only tables
//! and columns of the `public` schema are listed.

use sqlx::postgres::{PgPool, PgPoolOptions, PgRow};
use sqlx::{Column as _, Row as _, TypeInfo as _, ValueRef as _};

use super::error::DbError;
use super::schema::{ColumnMeta, TableKind, TableMeta};
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
    if lower.contains("password authentication failed") || lower.contains("role") {
        format!("authentication failed — {msg}")
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

/// Minimal introspection until FRE-11: `public` tables/views + columns from
/// information_schema (no PKs/indexes/FKs yet).
pub async fn introspect(pool: &PgPool) -> Result<Vec<TableMeta>, DbError> {
    let rows = sqlx::query(
        "SELECT table_name, table_type FROM information_schema.tables \
         WHERE table_schema = 'public' ORDER BY table_name",
    )
    .fetch_all(pool)
    .await
    .map_err(|e| DbError::Introspect(e.to_string()))?;

    let mut tables = Vec::with_capacity(rows.len());
    for row in rows {
        let name: String = get(&row, "table_name")?;
        let table_type: String = get(&row, "table_type")?;
        let kind = if table_type == "VIEW" {
            TableKind::View
        } else {
            TableKind::Table
        };
        tables.push(TableMeta {
            columns: table_columns(pool, &name).await?,
            indexes: Vec::new(),
            foreign_keys: Vec::new(),
            name,
            kind,
        });
    }
    Ok(tables)
}

async fn table_columns(pool: &PgPool, table: &str) -> Result<Vec<ColumnMeta>, DbError> {
    let rows = sqlx::query(
        "SELECT column_name, data_type, is_nullable, column_default \
         FROM information_schema.columns \
         WHERE table_schema = 'public' AND table_name = $1 \
         ORDER BY ordinal_position",
    )
    .bind(table)
    .fetch_all(pool)
    .await
    .map_err(|e| DbError::Introspect(e.to_string()))?;

    let mut columns = Vec::with_capacity(rows.len());
    for row in rows {
        let nullable: String = get(&row, "is_nullable")?;
        columns.push(ColumnMeta {
            name: get(&row, "column_name")?,
            type_name: get(&row, "data_type")?,
            nullable: nullable == "YES",
            primary_key_position: None,
            default: get::<Option<String>>(&row, "column_default")?,
        });
    }
    Ok(columns)
}

fn get<'r, T: sqlx::Decode<'r, sqlx::Postgres> + sqlx::Type<sqlx::Postgres>>(
    row: &'r PgRow,
    column: &str,
) -> Result<T, DbError> {
    row.try_get(column)
        .map_err(|e| DbError::Introspect(format!("column {column}: {e}")))
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
    }
}

/// Decodes the common scalar types; everything else falls back to a text
/// cast where possible. Full Postgres type rendering is FRE-12.
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
        "BOOL" => Value::Integer(row.try_get::<bool, _>(idx).map_err(map_err)? as i64),
        "INT2" => Value::Integer(row.try_get::<i16, _>(idx).map_err(map_err)? as i64),
        "INT4" => Value::Integer(row.try_get::<i32, _>(idx).map_err(map_err)? as i64),
        "INT8" => Value::Integer(row.try_get::<i64, _>(idx).map_err(map_err)?),
        "FLOAT4" => Value::Real(row.try_get::<f32, _>(idx).map_err(map_err)? as f64),
        "FLOAT8" => Value::Real(row.try_get::<f64, _>(idx).map_err(map_err)?),
        "TEXT" | "VARCHAR" | "BPCHAR" | "NAME" | "CHAR" => {
            Value::Text(row.try_get::<String, _>(idx).map_err(map_err)?)
        }
        "BYTEA" => Value::Blob(row.try_get::<Vec<u8>, _>(idx).map_err(map_err)?),
        _ => match row.try_get::<String, _>(idx) {
            Ok(text) => Value::Text(text),
            // Unknown type that won't decode as text; show a marker rather
            // than erroring the whole page (proper rendering is FRE-12).
            Err(_) => Value::Text(format!("<{}>", type_name.to_lowercase())),
        },
    };
    Ok(decoded)
}
