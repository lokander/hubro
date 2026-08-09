//! SQL Server backend on tiberius, split by concern: connecting, pooling and
//! statement execution live in [`pool`], the `CREATE TABLE`/`CREATE INDEX`
//! reconstruction in [`ddl`], and this file keeps what reads the database's
//! own description of itself — the saved-connection URL helpers, full schema
//! introspection from the sys catalog views, and row decoding into the
//! backend-neutral value model.
//!
//! Everything the rest of `db` calls is re-exported here, so callers keep
//! addressing `sqlserver::…` regardless of which file an item lives in.

mod ddl;
mod pool;

pub use ddl::fetch_ddl;
pub use pool::{
    begin_tx, commit_tx, execute, execute_all_checked, execute_conn, export, open_mssql,
    open_mssql_with, query_capped, query_capped_conn, query_with, rollback_tx, MssqlAuth,
    MssqlPool, MssqlTx,
};

use std::collections::HashMap;

use tiberius::{ColumnType, Row};

use super::error::DbError;
use super::schema::{
    ColumnMeta, ForeignKeyMeta, Generated, IndexMeta, TableKind, TableMeta, TypeDetail,
};
use super::value::{
    row_flag, row_int, row_opt_int, row_opt_text, row_text, trim_fraction, ColumnInfo, Value,
};

/// Splices a password into an mssql URL — see [`super::url::with_password`].
pub fn mssql_url_with_password(url: &str, password: &str) -> Result<String, DbError> {
    super::url::with_password(url, password)
}

/// Canonicalizes an mssql URL into the stable saved-connection locator — see
/// [`super::url::UrlScheme::normalize`] (`sqlserver://` → `mssql://`, default
/// port 1433).
pub fn normalize_mssql_url(url: &str) -> Result<String, DbError> {
    super::url::MSSQL.normalize(url)
}

/// The host and port an mssql URL points at (default port 1433) — see
/// [`super::url::UrlScheme::target`].
pub fn mssql_url_target(url: &str) -> Result<(String, u16), DbError> {
    super::url::MSSQL.target(url)
}

/// Rewrites a URL to connect through a forwarded local port — see
/// [`super::url::via_local_port`].
pub fn mssql_url_via_local_port(url: &str, port: u16) -> Result<String, DbError> {
    super::url::via_local_port(url, port)
}

/// Builds a password-free URL from the individual connection-form fields —
/// see [`super::url::UrlScheme::build`] (TLS param `encrypt`).
pub fn build_mssql_url(
    host: &str,
    port: &str,
    database: &str,
    user: &str,
    encrypt: &str,
) -> Result<String, DbError> {
    super::url::MSSQL.build(host, port, database, user, encrypt)
}

/// Full multi-schema introspection from the sys catalog views: every user
/// schema's tables and views with columns (PK membership, defaults,
/// identity/computed/rowversion detection), indexes, and foreign keys —
/// parity with the Postgres metadata model. Four batched queries regardless
/// of table count, grouped per (schema, table) in Rust.
///
/// The sys catalogs are used instead of INFORMATION_SCHEMA throughout:
/// INFORMATION_SCHEMA has no index view, loses multi-column FK ordering
/// guarantees, and lacks identity/computed flags — sys.objects with
/// `is_ms_shipped = 0` also gives a uniform system-object filter (which
/// excludes `sys`/`INFORMATION_SCHEMA` objects and shipped support tables).
///
/// Kind mapping: `'U'` → [`TableKind::Table`], `'V'` → [`TableKind::View`].
/// Indexed views map to plain `View` too — unlike a Postgres materialized
/// view they are transparently maintained (never stale, no REFRESH), so the
/// matview-specific handling doesn't apply; `MaterializedView` stays PG-only.
pub async fn introspect(pool: &MssqlPool) -> Result<Vec<TableMeta>, DbError> {
    let map_err = |e: DbError| DbError::Introspect(e.message().to_string());

    let table_rows = query_with(
        pool,
        "SELECT s.name, o.name, o.type \
         FROM sys.objects o \
         JOIN sys.schemas s ON s.schema_id = o.schema_id \
         WHERE o.type IN ('U', 'V') AND o.is_ms_shipped = 0 \
         ORDER BY s.name, o.name",
        &[],
    )
    .await
    .map_err(map_err)?;

    // Columns with PK positions resolved in SQL (LEFT JOIN on the primary-key
    // index's key columns), one row per column. sys.types is joined on
    // user_type_id so alias types report their alias name and system CLR
    // types (hierarchyid, geography, …) their own names. The raw
    // (max_length, precision, scale) triple is carried out and rendered into
    // the conventional readable form (`nvarchar(50)`, `decimal(10,2)`) by
    // [`format_mssql_type`]; default definitions are unwrapped from SQL
    // Server's parenthesis armor by [`strip_default_parens`].
    let column_rows = query_with(
        pool,
        "SELECT s.name, o.name, c.name, t.name, \
                c.max_length, c.precision, c.scale, \
                c.is_nullable, c.is_identity, c.is_computed, \
                d.definition, pk.key_ordinal \
         FROM sys.columns c \
         JOIN sys.objects o ON o.object_id = c.object_id \
         JOIN sys.schemas s ON s.schema_id = o.schema_id \
         JOIN sys.types t ON t.user_type_id = c.user_type_id \
         LEFT JOIN sys.default_constraints d \
           ON d.parent_object_id = c.object_id AND d.parent_column_id = c.column_id \
         LEFT JOIN ( \
             SELECT ic.object_id, ic.column_id, ic.key_ordinal \
             FROM sys.index_columns ic \
             JOIN sys.indexes i \
               ON i.object_id = ic.object_id AND i.index_id = ic.index_id \
             WHERE i.is_primary_key = 1 \
         ) pk ON pk.object_id = c.object_id AND pk.column_id = c.column_id \
         WHERE o.type IN ('U', 'V') AND o.is_ms_shipped = 0 \
         ORDER BY s.name, o.name, c.column_id",
        &[],
    )
    .await
    .map_err(map_err)?;

    // Indexes with their key columns in key order. Skips heaps (index_id 0),
    // hypothetical and disabled indexes (neither guarantees anything), and
    // non-key members (key_ordinal 0: INCLUDE columns, columnstore).
    // Filtered indexes (has_filter) map to `partial` — a filtered unique
    // index only guarantees uniqueness among matching rows, so row identity
    // must never rely on one (same contract as a Postgres partial index).
    let index_rows = query_with(
        pool,
        "SELECT s.name, o.name, i.name, i.is_unique, i.has_filter, col.name \
         FROM sys.indexes i \
         JOIN sys.objects o ON o.object_id = i.object_id \
         JOIN sys.schemas s ON s.schema_id = o.schema_id \
         JOIN sys.index_columns ic \
           ON ic.object_id = i.object_id AND ic.index_id = i.index_id \
         JOIN sys.columns col \
           ON col.object_id = ic.object_id AND col.column_id = ic.column_id \
         WHERE o.type IN ('U', 'V') AND o.is_ms_shipped = 0 \
           AND i.index_id > 0 AND i.is_hypothetical = 0 AND i.is_disabled = 0 \
           AND ic.key_ordinal > 0 \
         ORDER BY s.name, o.name, i.index_id, ic.key_ordinal",
        &[],
    )
    .await
    .map_err(map_err)?;

    // Foreign keys with the referencing/referenced column pairs in
    // constraint order (fkc.constraint_column_id). SQL Server always records
    // the referenced columns explicitly, so `referenced_columns` entries are
    // always `Some` — no implicit-PK resolution needed (fk.rs handles both).
    // Deliberately no `is_ms_shipped` filter here: an FK whose parent table
    // isn't in the user-table list fails the `table_index` lookup below and is
    // dropped anyway.
    let fk_rows = query_with(
        pool,
        "SELECT s.name, o.name, fk.name, rs.name, ro.name, pc.name, rc.name \
         FROM sys.foreign_keys fk \
         JOIN sys.objects o ON o.object_id = fk.parent_object_id \
         JOIN sys.schemas s ON s.schema_id = o.schema_id \
         JOIN sys.objects ro ON ro.object_id = fk.referenced_object_id \
         JOIN sys.schemas rs ON rs.schema_id = ro.schema_id \
         JOIN sys.foreign_key_columns fkc \
           ON fkc.constraint_object_id = fk.object_id \
         JOIN sys.columns pc \
           ON pc.object_id = fkc.parent_object_id AND pc.column_id = fkc.parent_column_id \
         JOIN sys.columns rc \
           ON rc.object_id = fkc.referenced_object_id AND rc.column_id = fkc.referenced_column_id \
         ORDER BY s.name, o.name, fk.name, fkc.constraint_column_id",
        &[],
    )
    .await
    .map_err(map_err)?;

    // The catalog columns below are read positionally with the shared row
    // accessors (`row_text`/`row_int`/`row_flag`) — sys.columns' `bit` flags
    // decode as Integer 0/1, which `row_flag` is exactly for.
    let mut tables: Vec<TableMeta> = Vec::with_capacity(table_rows.rows.len());
    for row in &table_rows.rows {
        tables.push(TableMeta {
            schema: Some(row_text(row, 0)),
            name: row_text(row, 1),
            // sys.objects.type is char(2), so the tag arrives padded.
            kind: match row_text(row, 2).trim() {
                "V" => TableKind::View,
                _ => TableKind::Table,
            },
            columns: Vec::new(),
            indexes: Vec::new(),
            foreign_keys: Vec::new(),
            // No per-object narrowing: kind and row identity already carry
            // everything this backend knows about writability (FRE-87).
            restriction: None,
            // SQL Server's system objects live in `sys`, which introspection
            // already skips; its partitioned tables are one object with
            // partitions underneath rather than child tables (FRE-88).
            internal: None,
            kind_label: None,
        });
    }
    // (schema, table) → index into `tables`, built once so grouping the
    // column/index/FK rows below is a hash lookup per row instead of a linear
    // scan over every table (FRE-133).
    let mut table_index: HashMap<(String, String), usize> = HashMap::with_capacity(tables.len());
    for (idx, table) in tables.iter().enumerate() {
        if let Some(schema) = &table.schema {
            table_index
                .entry((schema.clone(), table.name.clone()))
                .or_insert(idx);
        }
    }

    for row in &column_rows.rows {
        let schema = row_text(row, 0);
        let table = row_text(row, 1);
        let Some(&idx) = table_index.get(&(schema, table)) else {
            continue;
        };
        let type_base = row_text(row, 3);
        let default = row_opt_text(row, 10).map(|d| strip_default_parens(&d).to_string());
        // 0 is "not part of the primary key", not the first position.
        let pk_position = row_opt_int(row, 11).filter(|n| *n > 0).map(|n| n as u32);
        let generated = mssql_generated(row_flag(row, 8), row_flag(row, 9), &type_base);
        tables[idx].columns.push(ColumnMeta {
            name: row_text(row, 2),
            type_name: format_mssql_type(
                &type_base,
                row_int(row, 4),
                row_int(row, 5),
                row_int(row, 6),
            ),
            nullable: row_flag(row, 7),
            primary_key_position: pk_position,
            default,
            generated,
            // SQL Server has neither enum nor array types.
            type_detail: TypeDetail::Plain,
        });
    }

    for row in &index_rows.rows {
        let schema = row_text(row, 0);
        let table = row_text(row, 1);
        let Some(&idx) = table_index.get(&(schema, table)) else {
            continue;
        };
        let index_name = row_text(row, 2);
        let column = row_text(row, 5);
        let unique = row_flag(row, 3);
        let partial = row_flag(row, 4);
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

    // FK rows are ordered by (schema, table, fk name, key position); rows of
    // one constraint are contiguous, so track the last (table, name) pair to
    // append follow-up key columns to the entry that opened the constraint.
    let mut last_fk: Option<(usize, String)> = None;
    for row in &fk_rows.rows {
        let schema = row_text(row, 0);
        let table = row_text(row, 1);
        let Some(&idx) = table_index.get(&(schema, table)) else {
            continue;
        };
        let fk_name = row_text(row, 2);
        let column = row_text(row, 5);
        let ref_column = row_text(row, 6);
        let same = last_fk
            .as_ref()
            .is_some_and(|(i, name)| *i == idx && *name == fk_name);
        if same {
            let fk = tables[idx]
                .foreign_keys
                .last_mut()
                .expect("same-constraint row follows the entry that opened it");
            fk.columns.push(column);
            fk.referenced_columns.push(Some(ref_column));
        } else {
            tables[idx].foreign_keys.push(ForeignKeyMeta {
                columns: vec![column],
                referenced_schema: Some(row_text(row, 3)),
                referenced_table: row_text(row, 4),
                referenced_columns: vec![Some(ref_column)],
            });
            last_fk = Some((idx, fk_name));
        }
    }

    Ok(tables)
}

/// Renders a column's readable declared type the way SQL Server convention
/// writes it, from sys.columns' raw (max_length, precision, scale):
///
/// - char/varchar/binary/varbinary carry their byte length — `varchar(10)`,
///   `varbinary(max)` (max_length −1);
/// - nchar/nvarchar carry their *character* length, i.e. max_length halved
///   (UCS-2 stores two bytes per character) — `nvarchar(50)`;
/// - decimal/numeric carry `(precision,scale)`;
/// - time/datetime2/datetimeoffset carry their fractional-seconds scale
///   (SSMS always shows it, default 7) — `datetime2(7)`;
/// - everything else is the bare name (`int`, `bit`, `float`, `xml`, …).
fn format_mssql_type(base: &str, max_length: i64, precision: i64, scale: i64) -> String {
    match base {
        "char" | "varchar" | "binary" | "varbinary" => {
            if max_length == -1 {
                format!("{base}(max)")
            } else {
                format!("{base}({max_length})")
            }
        }
        "nchar" | "nvarchar" => {
            if max_length == -1 {
                format!("{base}(max)")
            } else {
                format!("{base}({})", max_length / 2)
            }
        }
        "decimal" | "numeric" => format!("{base}({precision},{scale})"),
        "time" | "datetime2" | "datetimeoffset" => format!("{base}({scale})"),
        _ => base.to_string(),
    }
}

/// Strips SQL Server's parenthesis wrapping from a default-constraint
/// definition: the catalog stores `0` as `((0))` and `getdate()` as
/// `(getdate())`. Each *fully wrapping* outer pair is removed — a pair only
/// counts when the opening paren's match is the final character, so
/// `((1)+(2))` unwraps once to `(1)+(2)` and no further. Paren counting is
/// textual (string literals aren't parsed), which can only err toward NOT
/// stripping — e.g. `('a)b')` is left as stored.
fn strip_default_parens(definition: &str) -> &str {
    let mut current = definition.trim();
    while wrapped_in_matching_parens(current) {
        current = current[1..current.len() - 1].trim();
    }
    current
}

/// Whether the string starts with `(` whose matching `)` is the last
/// character (false on unbalanced input).
fn wrapped_in_matching_parens(s: &str) -> bool {
    if !s.starts_with('(') {
        return false;
    }
    let mut depth = 0i64;
    for (i, c) in s.char_indices() {
        match c {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    return i == s.len() - 1;
                }
            }
            _ => {}
        }
    }
    false
}

/// Maps SQL Server's auto-assignment flavors onto [`Generated`] — every
/// flavor lands on [`Generated::Always`] (auto-assigned AND read-only in the
/// editor):
///
/// - computed columns are always database-assigned and never writable (like
///   Postgres `GENERATED ALWAYS AS (…) STORED`);
/// - rowversion (type name `timestamp` in sys.types, `rowversion` as the
///   modern alias) is stamped by the database on every write and never
///   user-writable;
/// - IDENTITY: an ordinary INSERT supplying an identity value fails (error
///   544 — the app never emits `SET IDENTITY_INSERT ON`), and UPDATE of an
///   identity column fails unconditionally (error 8102). Unlike a Postgres
///   `GENERATED BY DEFAULT AS IDENTITY`, the column is never writable
///   through the statements the editor produces, so `ByDefault` — which
///   leaves cells editable in the grid — would invite input that is doomed
///   to fail at commit.
fn mssql_generated(is_identity: bool, is_computed: bool, type_base: &str) -> Generated {
    if is_identity || is_computed || type_base == "timestamp" || type_base == "rowversion" {
        Generated::Always
    } else {
        Generated::Never
    }
}

fn column_infos(columns: &[tiberius::Column]) -> Vec<ColumnInfo> {
    columns
        .iter()
        .map(|c| ColumnInfo {
            name: c.name().to_string(),
        })
        .collect()
}

/// Decodes every cell of one fetched row into the backend-neutral [`Value`]
/// model.
fn decode_row(row: &Row) -> Vec<Value> {
    (0..row.len()).map(|idx| decode_value(row, idx)).collect()
}

/// Decodes scalar and rich SQL Server types into the backend-neutral
/// [`Value`] model. Rich types (dates, decimals, uuids, money, xml) render as
/// `Value::Text`, mirroring the Postgres backend's stringification style.
///
/// Cell data never errors the page: a type without a dedicated arm — or one
/// whose dedicated decode fails — degrades through [`decode_fallback`] to a
/// text read where possible, then to a `<typename>` marker.
fn decode_value(row: &Row, idx: usize) -> Value {
    let column_type = match row.columns().get(idx) {
        Some(column) => column.column_type(),
        None => ColumnType::Null,
    };
    decode_typed(row, idx, column_type).unwrap_or_else(|| decode_fallback(row, idx, column_type))
}

/// Shapes one `try_get` outcome: a decoded value, an SQL NULL, or `None` when
/// this arm cannot represent the cell (the caller then degrades).
fn opt<T>(result: tiberius::Result<Option<T>>, f: impl FnOnce(T) -> Value) -> Option<Value> {
    match result {
        Ok(Some(value)) => Some(f(value)),
        Ok(None) => Some(Value::Null),
        Err(_) => None,
    }
}

/// Type-specific decoding. Returns `None` both for types without a dedicated
/// arm and for values a dedicated arm cannot represent; the caller degrades
/// those via [`decode_fallback`].
fn decode_typed(row: &Row, idx: usize, column_type: ColumnType) -> Option<Value> {
    use chrono::{DateTime, FixedOffset, NaiveDate, NaiveDateTime, NaiveTime};
    match column_type {
        // bit renders as 0/1 — it IS numeric in T-SQL (no boolean literals).
        ColumnType::Bit | ColumnType::Bitn => {
            opt(row.try_get::<bool, _>(idx), |b| Value::Integer(b as i64))
        }
        ColumnType::Int1
        | ColumnType::Int2
        | ColumnType::Int4
        | ColumnType::Int8
        | ColumnType::Intn => decode_int(row, idx),
        ColumnType::Float4 | ColumnType::Float8 | ColumnType::Floatn => decode_float(row, idx),
        // money/smallmoney arrive from the driver as f64 (scaled by 1e-4);
        // render with the type's full 4-digit scale, matching how numeric
        // keeps its scale digits.
        ColumnType::Money | ColumnType::Money4 => opt(row.try_get::<f64, _>(idx), |m| {
            Value::Text(format!("{m:.4}"))
        }),
        // Exact decimal string from the driver's (i128 value, scale) pair —
        // must not round-trip through f64. tiberius's own Display is broken
        // for negative values, hence [`format_numeric`].
        ColumnType::Decimaln | ColumnType::Numericn => {
            opt(row.try_get::<tiberius::numeric::Numeric, _>(idx), |n| {
                Value::Text(format_numeric(&n))
            })
        }
        ColumnType::Guid => opt(row.try_get::<tiberius::Uuid, _>(idx), |u| {
            Value::Text(u.to_string())
        }),
        ColumnType::BigVarChar
        | ColumnType::BigChar
        | ColumnType::NVarchar
        | ColumnType::NChar
        | ColumnType::Text
        | ColumnType::NText => opt(row.try_get::<&str, _>(idx), |s| Value::Text(s.to_string())),
        ColumnType::BigVarBin | ColumnType::BigBinary | ColumnType::Image => {
            opt(row.try_get::<&[u8], _>(idx), |b| Value::Blob(b.to_vec()))
        }
        ColumnType::Xml => opt(row.try_get::<&tiberius::xml::XmlData, _>(idx), |x| {
            Value::Text(x.to_string())
        }),
        // Date/time family; `%.f` prints fractional seconds only when
        // non-zero, and trailing zeros are trimmed, matching the Postgres
        // backend's rendering. datetime2 carries up to 7 fractional digits
        // (100 ns), which chrono's nanosecond precision covers exactly.
        ColumnType::Datetime2 => opt(row.try_get::<NaiveDateTime, _>(idx), |ts| {
            Value::Text(trim_fraction(ts.format("%Y-%m-%d %H:%M:%S%.f").to_string()))
        }),
        // Legacy datetime/smalldatetime tick in 1/300 s, which the driver
        // converts to a repeating-decimal nanosecond value (".336666666");
        // round to the type's actual millisecond display precision (".337")
        // the way SQL Server itself prints it.
        ColumnType::Datetime | ColumnType::Datetime4 | ColumnType::Datetimen => {
            opt(row.try_get::<NaiveDateTime, _>(idx), |ts| {
                use chrono::SubsecRound as _;
                let ts = ts.round_subsecs(3);
                Value::Text(trim_fraction(ts.format("%Y-%m-%d %H:%M:%S%.f").to_string()))
            })
        }
        ColumnType::Daten => opt(row.try_get::<NaiveDate, _>(idx), |d| {
            Value::Text(d.format("%Y-%m-%d").to_string())
        }),
        ColumnType::Timen => opt(row.try_get::<NaiveTime, _>(idx), |t| {
            Value::Text(trim_fraction(t.format("%H:%M:%S%.f").to_string()))
        }),
        // datetimeoffset keeps its stored offset (it is real data, unlike
        // Postgres's timestamptz which the server sends as an instant).
        ColumnType::DatetimeOffsetn => opt(row.try_get::<DateTime<FixedOffset>, _>(idx), |dt| {
            let local = trim_fraction(dt.format("%Y-%m-%d %H:%M:%S%.f").to_string());
            Value::Text(format!("{local}{}", dt.format("%:z")))
        }),
        // sql_variant / UDT / unknown: no dedicated arm.
        _ => None,
    }
}

/// The integer tiers behind int/bigint/smallint/tinyint (and their nullable
/// `intn` wire form): the driver reports the *declared* width in the column
/// type but sends each cell at its actual width, so try widest-first.
fn decode_int(row: &Row, idx: usize) -> Option<Value> {
    if let Some(v) = opt(row.try_get::<i64, _>(idx), Value::Integer) {
        return Some(v);
    }
    if let Some(v) = opt(row.try_get::<i32, _>(idx), |i| Value::Integer(i as i64)) {
        return Some(v);
    }
    if let Some(v) = opt(row.try_get::<i16, _>(idx), |i| Value::Integer(i as i64)) {
        return Some(v);
    }
    opt(row.try_get::<u8, _>(idx), |i| Value::Integer(i as i64))
}

/// float(53)/float(24) and their nullable `floatn` wire form.
fn decode_float(row: &Row, idx: usize) -> Option<Value> {
    if let Some(v) = opt(row.try_get::<f64, _>(idx), Value::Real) {
        return Some(v);
    }
    opt(row.try_get::<f32, _>(idx), |f| Value::Real(f as f64))
}

/// Graceful degradation for values [`decode_typed`] can't produce: a text
/// read, then a `<typename>` marker. Infallible by design — one odd cell
/// must not take down the page.
fn decode_fallback(row: &Row, idx: usize, column_type: ColumnType) -> Value {
    if let Some(value) = opt(row.try_get::<&str, _>(idx), |s| Value::Text(s.to_string())) {
        return value;
    }
    Value::Text(format!("<{}>", format!("{column_type:?}").to_lowercase()))
}

/// Exact decimal string for a numeric/decimal value from its scaled i128 and
/// scale, keeping the full scale digits (`1.50` stays `1.50`), like the
/// Postgres backend's NUMERIC stringification. Hand-rolled because
/// `tiberius::numeric::Numeric`'s own Display mangles negative values
/// (it formats the integer and fraction parts independently, each with its
/// own minus sign).
fn format_numeric(n: &tiberius::numeric::Numeric) -> String {
    let scale = n.scale() as usize;
    let value = n.value();
    let sign = if value < 0 { "-" } else { "" };
    let digits = value.unsigned_abs().to_string();
    if scale == 0 {
        return format!("{sign}{digits}");
    }
    let padded = format!("{digits:0>width$}", width = scale + 1);
    let (int_part, frac_part) = padded.split_at(padded.len() - scale);
    format!("{sign}{int_part}.{frac_part}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_wrappers_bind_the_mssql_scheme() {
        // One probe per wrapper; the shared behavior is covered in db::url.
        assert_eq!(
            mssql_url_with_password("mssql://sa@db.example.com:1433/app", "p%rd").unwrap(),
            "mssql://sa:p%25rd@db.example.com:1433/app"
        );
        assert_eq!(
            normalize_mssql_url("sqlserver://user@Db.Example.COM/app").unwrap(),
            "mssql://user@db.example.com:1433/app"
        );
        assert_eq!(
            mssql_url_target("mssql://u@db.internal/app").unwrap(),
            ("db.internal".to_string(), 1433)
        );
        assert_eq!(
            mssql_url_via_local_port("mssql://u@db.internal:1433/app?encrypt=off", 40123).unwrap(),
            "mssql://u@127.0.0.1:40123/app?encrypt=off"
        );
        assert_eq!(
            build_mssql_url("db.example.com", "", "app", "sa", "on").unwrap(),
            "mssql://sa@db.example.com:1433/app?encrypt=on"
        );
    }

    #[test]
    fn format_numeric_keeps_scale_and_handles_negatives() {
        let n = |value: i128, scale: u8| tiberius::numeric::Numeric::new_with_scale(value, scale);
        assert_eq!(format_numeric(&n(12345, 2)), "123.45");
        assert_eq!(format_numeric(&n(-15, 1)), "-1.5");
        assert_eq!(format_numeric(&n(5, 3)), "0.005");
        assert_eq!(format_numeric(&n(-5, 3)), "-0.005");
        assert_eq!(format_numeric(&n(0, 2)), "0.00");
        assert_eq!(format_numeric(&n(1500, 2)), "15.00");
        assert_eq!(format_numeric(&n(42, 0)), "42");
        assert_eq!(format_numeric(&n(-42, 0)), "-42");
        // 38-digit values stay exact (i128 range, beyond f64 and
        // rust_decimal).
        assert_eq!(
            format_numeric(&n(
                99_999_999_999_999_999_999_999_999_999_999_999_999i128,
                4
            )),
            "9999999999999999999999999999999999.9999"
        );
    }

    #[test]
    fn format_mssql_type_renders_conventional_readable_names() {
        // Byte-length family; -1 is (max).
        assert_eq!(format_mssql_type("varchar", 10, 0, 0), "varchar(10)");
        assert_eq!(format_mssql_type("varchar", -1, 0, 0), "varchar(max)");
        assert_eq!(format_mssql_type("char", 3, 0, 0), "char(3)");
        assert_eq!(format_mssql_type("binary", 16, 0, 0), "binary(16)");
        assert_eq!(format_mssql_type("varbinary", -1, 0, 0), "varbinary(max)");
        // UCS-2 family: max_length is bytes, the readable length characters.
        assert_eq!(format_mssql_type("nvarchar", 100, 0, 0), "nvarchar(50)");
        assert_eq!(format_mssql_type("nvarchar", -1, 0, 0), "nvarchar(max)");
        assert_eq!(format_mssql_type("nchar", 20, 0, 0), "nchar(10)");
        // Exact numerics carry (precision,scale).
        assert_eq!(format_mssql_type("decimal", 9, 10, 2), "decimal(10,2)");
        assert_eq!(format_mssql_type("numeric", 9, 18, 0), "numeric(18,0)");
        // Fractional-seconds family carries its scale.
        assert_eq!(format_mssql_type("datetime2", 8, 27, 7), "datetime2(7)");
        assert_eq!(format_mssql_type("time", 5, 16, 3), "time(3)");
        assert_eq!(
            format_mssql_type("datetimeoffset", 10, 34, 7),
            "datetimeoffset(7)"
        );
        // Everything else is the bare name.
        for bare in [
            "int",
            "bigint",
            "bit",
            "float",
            "real",
            "money",
            "date",
            "datetime",
            "uniqueidentifier",
            "xml",
            "timestamp",
            "sql_variant",
            "hierarchyid",
            "geography",
        ] {
            assert_eq!(format_mssql_type(bare, 8, 0, 0), bare);
        }
    }

    #[test]
    fn strip_default_parens_unwraps_only_full_wrapping_pairs() {
        assert_eq!(strip_default_parens("((0))"), "0");
        assert_eq!(strip_default_parens("(getdate())"), "getdate()");
        assert_eq!(strip_default_parens("(N'abc')"), "N'abc'");
        assert_eq!(strip_default_parens("((-1))"), "-1");
        // Only pairs wrapping the WHOLE expression unwrap.
        assert_eq!(strip_default_parens("((1)+(2))"), "(1)+(2)");
        // Already bare / non-wrapping input passes through.
        assert_eq!(strip_default_parens("0"), "0");
        assert_eq!(strip_default_parens("(1)+(2)"), "(1)+(2)");
        // Textual paren counting doesn't parse string literals; the failure
        // mode is conservative (no strip), never over-stripping.
        assert_eq!(strip_default_parens("('a)b')"), "('a)b')");
        // Unbalanced input is left alone.
        assert_eq!(strip_default_parens("((0)"), "((0)");
    }

    #[test]
    fn generated_maps_identity_computed_and_rowversion() {
        // Ordinary column.
        assert_eq!(mssql_generated(false, false, "int"), Generated::Never);
        // IDENTITY is read-only through the statements the editor produces
        // (INSERT with a value fails without IDENTITY_INSERT, UPDATE always
        // fails) — Always, matching how staging models identity columns.
        assert_eq!(mssql_generated(true, false, "int"), Generated::Always);
        // Computed columns are always database-assigned.
        assert_eq!(mssql_generated(false, true, "int"), Generated::Always);
        // rowversion is stamped on every write (sys.types calls it
        // `timestamp`; `rowversion` is the modern alias).
        assert_eq!(
            mssql_generated(false, false, "timestamp"),
            Generated::Always
        );
        assert_eq!(
            mssql_generated(false, false, "rowversion"),
            Generated::Always
        );
        // A datetime column named after the legacy type is NOT rowversion —
        // the match is on the exact catalog type name, which is fine because
        // sys.types never reports `timestamp` for anything else.
        assert_eq!(mssql_generated(false, false, "datetime2"), Generated::Never);
    }
}
