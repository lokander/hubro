//! SQLite backend: connecting, introspection, and query execution.

use std::path::Path;

use sqlx::sqlite::{SqliteConnectOptions, SqlitePool, SqlitePoolOptions, SqliteRow};
use sqlx::{Column as _, Row as _, TypeInfo as _, ValueRef as _};

use super::error::DbError;
use super::page::quote_ident;
use super::schema::{ColumnMeta, ForeignKeyMeta, IndexMeta, TableKind, TableMeta};
use super::value::{ColumnInfo, QueryResult, Value};

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

/// Like [`query`], with bound parameters (used by the paged table reader so
/// filter values never touch the SQL text).
pub async fn query_with(
    pool: &SqlitePool,
    sql: &str,
    params: &[Value],
) -> Result<QueryResult, DbError> {
    let mut prepared = sqlx::query(sql);
    for param in params {
        prepared = match param {
            Value::Null => prepared.bind(None::<i64>),
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
            columns: table_columns(pool, &name).await?,
            indexes: table_indexes(pool, &name).await?,
            foreign_keys: table_foreign_keys(pool, &name).await?,
            name,
            kind,
        });
    }
    Ok(tables)
}

async fn table_columns(pool: &SqlitePool, table: &str) -> Result<Vec<ColumnMeta>, DbError> {
    let rows = pragma(pool, "table_info", table).await?;
    let mut columns = Vec::with_capacity(rows.len());
    for row in rows {
        let pk: i64 = get(&row, "pk")?;
        let notnull: i64 = get(&row, "notnull")?;
        columns.push(ColumnMeta {
            name: get(&row, "name")?,
            type_name: get(&row, "type")?,
            nullable: notnull == 0,
            primary_key_position: (pk > 0).then_some(pk as u32),
            default: get::<Option<String>>(&row, "dflt_value")?,
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
        let column_rows = pragma(pool, "index_info", &name).await?;
        let mut columns: Vec<(i64, Option<String>)> = Vec::with_capacity(column_rows.len());
        for col in column_rows {
            columns.push((get(&col, "seqno")?, get(&col, "name")?));
        }
        columns.sort_by_key(|(seqno, _)| *seqno);
        indexes.push(IndexMeta {
            name,
            unique: unique != 0,
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
