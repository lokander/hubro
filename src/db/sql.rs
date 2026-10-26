//! The dialect toolkit shared by every SQL builder: placeholder and cast
//! rendering, identifier quoting and schema qualification, the "filters
//! compare as text" comparison strategy, and the bounded-preview
//! `(value, length)` expression pairs.
//!
//! Identifier names come from introspection (still quoted defensively);
//! values are always bound parameters, never interpolated. Everything here is
//! pure string building — no driver types, no I/O.

use super::page::ColumnClass;
use super::schema::TableMeta;
use super::value::Value;

/// SQL flavor differences the SQL builders must care about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dialect {
    Sqlite,
    Postgres,
    SqlServer,
}

impl Dialect {
    /// How this dialect caps a SELECT at `limit` rows, as the text that goes
    /// right after `SELECT` and the text that ends the statement — the one
    /// place either spelling lives.
    ///
    /// With no `offset`, the first `limit` rows: ` LIMIT n` on SQLite and
    /// Postgres, `TOP n` on SQL Server, which puts the cap right after
    /// `SELECT`. With an `offset`, a page: ` LIMIT n OFFSET m`, or on SQL
    /// Server ` OFFSET m ROWS FETCH NEXT n ROWS ONLY`, which is valid only
    /// after an `ORDER BY` — so when the statement has none (`has_order`
    /// false), the tail supplies a synthetic ` ORDER BY (SELECT NULL)` first,
    /// keeping the SQL valid without imposing an order.
    pub(crate) fn row_cap(
        self,
        limit: u64,
        offset: Option<u64>,
        has_order: bool,
    ) -> (String, String) {
        match (self, offset) {
            (Dialect::Sqlite | Dialect::Postgres, None) => {
                (String::new(), format!(" LIMIT {limit}"))
            }
            (Dialect::Sqlite | Dialect::Postgres, Some(offset)) => {
                (String::new(), format!(" LIMIT {limit} OFFSET {offset}"))
            }
            (Dialect::SqlServer, None) => (format!("TOP {limit} "), String::new()),
            (Dialect::SqlServer, Some(offset)) => {
                let order = if has_order {
                    ""
                } else {
                    " ORDER BY (SELECT NULL)"
                };
                (
                    String::new(),
                    format!("{order} OFFSET {offset} ROWS FETCH NEXT {limit} ROWS ONLY"),
                )
            }
        }
    }

    /// The `n`th bound-parameter placeholder (1-based): SQLite placeholders
    /// are positional so every one renders as `?`; Postgres numbers them
    /// `$n`; SQL Server numbers them `@Pn` (the tiberius convention). Every
    /// SQL builder routes its placeholders through here.
    pub(crate) fn placeholder(self, n: usize) -> String {
        match self {
            Dialect::Sqlite => "?".to_string(),
            Dialect::Postgres => format!("${n}"),
            Dialect::SqlServer => format!("@P{n}"),
        }
    }

    /// Renders a cast of `expr` (already-safe SQL, e.g. a quoted identifier
    /// or a placeholder) to `target` (an introspected/static type name).
    /// Postgres uses the postfix `expr::type` shape; SQLite and SQL Server
    /// the prefix `CAST(expr AS type)` one. SQLite has no `::` operator at
    /// all ("unrecognized token"), so although no builder casts there today
    /// (its type affinity coerces on its own), the shape it gets is one it
    /// can run.
    ///
    /// Callers pass a **dialect-neutral** target for the generic cases and
    /// this method translates it: the stringify target `"text"` becomes
    /// `nvarchar(max)` on SQL Server (T-SQL has no `text`-as-cast-target
    /// worth using — the legacy `text` type is deprecated and not
    /// comparable). Any other target, and `text` on SQLite, renders verbatim.
    pub(crate) fn cast_expr(self, expr: &str, target: &str) -> String {
        match self {
            Dialect::Postgres => format!("{expr}::{target}"),
            Dialect::Sqlite => format!("CAST({expr} AS {target})"),
            Dialect::SqlServer => {
                let target = if target == "text" {
                    "nvarchar(max)"
                } else {
                    target
                };
                format!("CAST({expr} AS {target})")
            }
        }
    }
}

/// Double-quotes an identifier, doubling embedded quotes. Deliberately takes
/// no [`Dialect`]: ANSI `"…"` quoting works on SQLite, Postgres, and SQL
/// Server (with QUOTED_IDENTIFIER ON, which tiberius defaults to), so one
/// dialect-independent form covers every backend we would add.
pub(crate) fn quote_ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// Schema-qualified, quoted object name (`None` schema for SQLite / default
/// resolution). The one spelling every builder shares — page selects, staged
/// writes, cell fetches, DDL reconstruction, and clipboard INSERTs all
/// qualify through here.
pub(crate) fn qualified(schema: Option<&str>, name: &str) -> String {
    match schema {
        Some(schema) => format!("{}.{}", quote_ident(schema), quote_ident(name)),
        None => quote_ident(name),
    }
}

/// The left-hand side of a text comparison against `column` — the "filters
/// compare as text" strategy of the filter bar: Postgres and SQL Server
/// compare strictly by type, so the column is cast to text to match the text
/// the user typed, letting exotic types (uuid, enums, timestamps) still
/// compare. SQLite's affinity coerces implicitly, so the bare quoted column
/// suffices there. Fine for a viewer; indexes aren't a concern yet.
///
/// **Only for text the user typed.** A value read off a row — a foreign-key
/// jump's, a row's own key — is typed, and comparing it as text depends on
/// hubro spelling every value exactly as the server does, which it does not
/// for floats, blobs, `timestamptz`, padded `character(n)` or SQL Server's
/// date/time types. Those compare through `staged::equalities_where` and
/// `staged::row_where`.
pub(crate) fn text_compare_expr(dialect: Dialect, column: &str) -> String {
    match dialect {
        Dialect::Sqlite => quote_ident(column),
        Dialect::Postgres | Dialect::SqlServer => dialect.cast_expr(&quote_ident(column), "text"),
    }
}

/// Length of a SQL Server text value in the units `SUBSTRING` slices by.
/// `cast` must already be an `nvarchar(max)` expression.
///
/// **Not `LEN()`.** A bounded read is only safe while the length probe and
/// the slice count the same thing, because "was this truncated?" is decided
/// by comparing one against the other. `LEN` breaks that invariant: it
/// ignores trailing spaces, `SUBSTRING` does not. So an `nvarchar` of 3000
/// characters ending in 1000 spaces reports `LEN = 2000`, the reader sees
/// 2000 ≤ 2048 and records no [`PreviewInfo`](super::page::PreviewInfo) —
/// and the 2048-character prefix it actually fetched is then treated as the
/// whole value. That silently truncates a clipboard copy, and worse, an
/// inline edit of that cell saves the prefix back over the real data.
///
/// `DATALENGTH` is bytes and `nvarchar` is UTF-16, so `/ 2` gives code units
/// — exactly what `SUBSTRING` counts, and always an exact division.
///
/// Two consequences, both correct, both confined to SQL Server text columns:
///
/// - A fixed-width `nchar(n)`/`char(n)` column wider than the cap now always
///   reads as truncated, because the engine really does store — and slice —
///   the full padded width. Verified: `nchar(20)` holding `N'ab'` reports
///   `LEN = 2`, while `SUBSTRING(…, 1, 20)` returns all 20 units. Such a cell
///   used to render as complete while silently missing its padding.
/// - Under a supplementary-character (`_SC`) collation `SUBSTRING` counts an
///   astral character as one while this counts its two code units, so the
///   probe can *over*-report. That direction is harmless: it marks a complete
///   value as a preview, costing one redundant fetch that then returns the
///   full value. Only under-reporting loses data.
pub(crate) fn mssql_text_len(cast: &str) -> String {
    format!("DATALENGTH({cast}) / 2")
}

/// The bounded-preview `(value_expr, length_expr)` pair for one column:
/// the value expression fetches at most `cap` characters (text) or
/// bytes (binary) of `q` (an already-quoted column identifier), and the
/// length expression probes the full stored length in the *same units the
/// slice counts* — the invariant that decides "was this truncated?"
/// (see [`mssql_text_len`] for why SQL Server text must not use `LEN`).
/// Scalars are fetched whole with a `NULL` length placeholder (previewing a
/// scalar would corrupt its decoded type). Shared by the grid's bounded page
/// select and the on-demand cell fetch so the two paths can never disagree
/// about truncation.
pub(crate) fn preview_exprs(
    dialect: Dialect,
    class: ColumnClass,
    q: &str,
    cap: i64,
) -> (String, String) {
    match (dialect, class) {
        (_, ColumnClass::Scalar) => (q.to_string(), "NULL".to_string()),
        // SQLite length() is characters for text, bytes for blobs; substr
        // slices the same way.
        (Dialect::Sqlite, _) => (format!("substr({q}, 1, {cap})"), format!("length({q})")),
        (Dialect::Postgres, ColumnClass::Text) => {
            // Cast to text so json/xml/user-defined types preview too.
            let cast = dialect.cast_expr(q, "text");
            (format!("left({cast}, {cap})"), format!("length({cast})"))
        }
        (Dialect::Postgres, ColumnClass::Binary) => (
            format!("substring({q} from 1 for {cap})"),
            format!("octet_length({q})"),
        ),
        (Dialect::SqlServer, ColumnClass::Text) => {
            // Cast to nvarchar(max) so xml/legacy types preview too. The
            // length probe counts UTF-16 code units, matching what SUBSTRING
            // slices — see `mssql_text_len` for why `LEN` would silently
            // truncate.
            let cast = dialect.cast_expr(q, "text");
            (
                format!("SUBSTRING({cast}, 1, {cap})"),
                mssql_text_len(&cast),
            )
        }
        (Dialect::SqlServer, ColumnClass::Binary) => (
            // SUBSTRING works on varbinary; DATALENGTH is bytes.
            format!("SUBSTRING({q}, 1, {cap})"),
            format!("DATALENGTH({q})"),
        ),
    }
}

/// Builds the single-row SELECT [`DbPool::fetch_cell`](super::DbPool::fetch_cell)
/// runs: the bounded [`preview_exprs`] pair for `column` (capped at `cap`
/// characters/bytes), the row-pinning `where_clause`, and a one-row cap
/// ([`Dialect::row_cap`]).
pub(crate) fn cell_fetch_sql(
    dialect: Dialect,
    table: &TableMeta,
    class: ColumnClass,
    column: &str,
    where_clause: &str,
    cap: usize,
) -> String {
    let (value_expr, length_expr) = preview_exprs(dialect, class, &quote_ident(column), cap as i64);
    let (top, tail) = dialect.row_cap(1, None, false);
    format!(
        "SELECT {top}{value_expr}, {length_expr} FROM {}{where_clause}{tail}",
        qualified(table.schema.as_deref(), &table.name),
    )
}

/// Fallback full-length for a value when the `length` column came back NULL
/// or non-integer (chars for text, bytes for blob, else 0).
pub(crate) fn value_len(value: &Value) -> u64 {
    match value {
        Value::Text(t) => t.chars().count() as u64,
        Value::Blob(b) => b.len() as u64,
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::schema::TableKind;
    use crate::db::FETCH_CELL_MAX_BYTES;

    fn cell_table(schema: Option<&str>) -> TableMeta {
        TableMeta {
            schema: schema.map(str::to_string),
            name: "t".into(),
            kind: TableKind::Table,
            columns: vec![],
            indexes: vec![],
            foreign_keys: vec![],
            restriction: None,
            internal: None,
            kind_label: None,
        }
    }

    #[tokio::test]
    async fn sqlite_casts_are_a_shape_sqlite_runs() {
        // Measured: SQLite rejects the postfix `?::integer` ("unrecognized
        // token"), so the shape is pinned and then run against the engine.
        assert_eq!(
            Dialect::Sqlite.cast_expr("\"c\"", "text"),
            "CAST(\"c\" AS text)"
        );
        let pool = sqlx::SqlitePool::connect("sqlite::memory:").await.unwrap();
        for target in ["integer", "real", "text"] {
            let cast = Dialect::Sqlite.cast_expr(&Dialect::Sqlite.placeholder(1), target);
            let sql = format!("SELECT typeof({cast})");
            let storage: String = sqlx::query_scalar(&sql)
                .bind("42")
                .fetch_one(&pool)
                .await
                .unwrap_or_else(|e| panic!("{sql}: {e}"));
            assert_eq!(storage, target, "{sql}");
        }
    }

    #[test]
    fn qualified_quotes_both_halves() {
        assert_eq!(qualified(None, "tracks"), "\"tracks\"");
        assert_eq!(
            qualified(Some("app data"), "tracks"),
            "\"app data\".\"tracks\""
        );
        assert_eq!(qualified(Some("s"), "na\"me"), "\"s\".\"na\"\"me\"");
    }

    #[test]
    fn cell_fetch_sql_bounds_previews_per_dialect() {
        let cap = FETCH_CELL_MAX_BYTES as i64;
        let table = cell_table(None);
        // SQLite: substr/length for text and binary alike.
        assert_eq!(
            cell_fetch_sql(
                Dialect::Sqlite,
                &table,
                ColumnClass::Text,
                "c",
                " WHERE \"id\" = ?",
                FETCH_CELL_MAX_BYTES,
            ),
            format!(
                "SELECT substr(\"c\", 1, {cap}), length(\"c\") FROM \"t\" WHERE \"id\" = ? LIMIT 1"
            )
        );
        // Postgres: text cast routed through cast_expr.
        assert_eq!(
            cell_fetch_sql(
                Dialect::Postgres,
                &table,
                ColumnClass::Text,
                "c",
                " WHERE \"id\" = $1::integer",
                FETCH_CELL_MAX_BYTES,
            ),
            format!(
                "SELECT left(\"c\"::text, {cap}), length(\"c\"::text) \
                 FROM \"t\" WHERE \"id\" = $1::integer LIMIT 1"
            )
        );
        assert_eq!(
            cell_fetch_sql(
                Dialect::Postgres,
                &table,
                ColumnClass::Binary,
                "c",
                "",
                FETCH_CELL_MAX_BYTES,
            ),
            format!(
                "SELECT substring(\"c\" from 1 for {cap}), octet_length(\"c\") \
                 FROM \"t\" LIMIT 1"
            )
        );
        // Scalars are fetched whole, with a NULL length placeholder.
        assert_eq!(
            cell_fetch_sql(
                Dialect::Sqlite,
                &table,
                ColumnClass::Scalar,
                "c",
                "",
                FETCH_CELL_MAX_BYTES,
            ),
            "SELECT \"c\", NULL FROM \"t\" LIMIT 1"
        );
    }

    #[test]
    fn cell_fetch_sql_uses_top_1_and_tsql_functions_on_sqlserver() {
        let cap = FETCH_CELL_MAX_BYTES as i64;
        let table = cell_table(Some("dbo"));
        // Text: SUBSTRING preview + code-unit length over an nvarchar cast;
        // TOP 1 right after SELECT, no LIMIT tail.
        assert_eq!(
            cell_fetch_sql(
                Dialect::SqlServer,
                &table,
                ColumnClass::Text,
                "c",
                " WHERE \"id\" = @P1",
                FETCH_CELL_MAX_BYTES,
            ),
            format!(
                "SELECT TOP 1 SUBSTRING(CAST(\"c\" AS nvarchar(max)), 1, {cap}), \
                 DATALENGTH(CAST(\"c\" AS nvarchar(max))) / 2 \
                 FROM \"dbo\".\"t\" WHERE \"id\" = @P1"
            )
        );
        // Binary: raw SUBSTRING + DATALENGTH (bytes).
        assert_eq!(
            cell_fetch_sql(
                Dialect::SqlServer,
                &table,
                ColumnClass::Binary,
                "c",
                "",
                FETCH_CELL_MAX_BYTES,
            ),
            format!(
                "SELECT TOP 1 SUBSTRING(\"c\", 1, {cap}), DATALENGTH(\"c\") FROM \"dbo\".\"t\""
            )
        );
        // Scalar: whole value, still TOP 1.
        assert_eq!(
            cell_fetch_sql(
                Dialect::SqlServer,
                &table,
                ColumnClass::Scalar,
                "c",
                "",
                FETCH_CELL_MAX_BYTES,
            ),
            "SELECT TOP 1 \"c\", NULL FROM \"dbo\".\"t\""
        );
    }
}
