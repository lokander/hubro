//! SQL generation for paged table reads with optional sort and filter.
//! Identifier names come from introspection (still quoted defensively);
//! filter values are always bound parameters, never interpolated.
//!
//! Two page shapes exist:
//!
//! - [`PageRequest::select_sql`] fetches every column in full (`SELECT *`).
//! - [`PageRequest::select_bounded_sql`] fetches a bounded *preview* of large
//!   columns (long text / json / blobs) instead of their full contents, so a
//!   page of multi-MB values never buffers megabytes per row. Small
//!   columns and identity/foreign-key columns are always fetched whole so row
//!   addressing and navigation stay exact.

use super::schema::ColumnMeta;
use super::sql::{preview_exprs, qualified, quote_ident, text_compare_expr, Dialect};
use super::staged::equalities_where;
use super::value::Value;

/// Max characters (text/json) or bytes (blob) of a large-cell preview fetched
/// into a grid page. A value longer than this is truncated to a preview and
/// flagged; the full value is loaded lazily on cell expand
/// ([`super::DbPool::fetch_cell`]). 2 KiB comfortably shows a paragraph while
/// keeping a 100-row page of huge cells to a couple hundred KiB.
pub const PREVIEW_BYTES: usize = 2048;

/// How a column is treated by a bounded page fetch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColumnClass {
    /// Fixed-size scalar (int/real/bool/date/uuid/…): fetched whole.
    Scalar,
    /// Text-like (text/varchar/char/clob/json/xml, or an unknown/empty
    /// declared type): fetched as a bounded text preview.
    Text,
    /// Binary (blob/bytea): fetched as a bounded byte preview; the grid only
    /// ever shows its size, and the full length comes from a companion
    /// `length` column.
    Binary,
}

/// Classifies a declared column type into a [`ColumnClass`]. The binary and
/// scalar vocabularies match on the **normalized base name** — case ignored,
/// any `(n)`/`(max)` parameter suffix dropped, `unsigned`/`signed`
/// qualifiers dropped — never by substring; only the text fallback matches
/// by substring, because Text is the safe direction (it previews). Every
/// recognized scalar is fetched whole (previewing a scalar would corrupt its
/// decoded type and, worse, any locator built from it); anything
/// unrecognized previews as Text.
pub fn classify_column(type_name: &str) -> ColumnClass {
    let t = type_name.trim().to_ascii_lowercase();
    if t.contains("blob") || t.contains("bytea") {
        return ColumnClass::Binary;
    }
    // SQL Server binary types match by exact base name (any `(n)`/`(max)`
    // parameter suffix stripped), NOT by substring: Postgres type names
    // include user-defined types, and an enum/domain merely *containing*
    // these words (an `image_format` enum, a `binary_state` domain) must
    // stay Text — a Binary classification would emit byte-preview SQL
    // (`substring(… from … for …)`, `octet_length`) that errors on such
    // columns and makes the whole table unbrowsable.
    let base = t.split('(').next().unwrap_or("").trim_end();
    if matches!(base, "binary" | "varbinary" | "image") {
        return ColumnClass::Binary;
    }
    // A declared scalar keeps its native decoded type. Checked before the
    // text fallback so an empty/unknown type still previews.
    if is_scalar_base(base) {
        return ColumnClass::Scalar;
    }
    if t.contains("char")
        || t.contains("text")
        || t.contains("clob")
        || t.contains("json")
        || t.contains("xml")
        || t.is_empty()
    {
        return ColumnClass::Text;
    }
    // Everything else previews, and that is a decision, not an oversight:
    //
    // - **User-defined types.** They can hold arbitrarily large text, so
    //   buffering one in full would escape the preview memory bound. This is
    //   why the vocabularies above match the whole normalized name instead
    //   of a substring — a type merely *containing* a scalar word must not
    //   be fetched whole. The names really do reach here: a materialized
    //   view's columns come from `format_type()`, which reports the user
    //   type itself (`app.timeline`, `app.fingerprint_data`); SQL Server
    //   alias types report their alias name from `sys.types`; and SQLite
    //   declared types are free-form text. (On an ordinary Postgres table
    //   `data_type` says `USER-DEFINED` for an enum and the *base* type for
    //   a domain, so that path never reaches here under a user name.)
    // - **Postgres range and multirange types** (`int4range`, `daterange`,
    //   `numrange`, `tstzrange`, `int8multirange`, …), which arrive as
    //   `data_type` verbatim. They preview *uniformly* — the text render is
    //   what the grid would show either way, and one rule beats the split
    //   this vocabulary used to fall into accidentally (`int4range` matched
    //   the `int` substring while `numrange` matched nothing).
    // - **Array types**, whether they arrive as `ARRAY` from a table or as
    //   `integer[]`/`uuid[]` from a materialized view's `format_type()` —
    //   both spellings preview, so the two introspection paths agree.
    ColumnClass::Text
}

/// Whether a normalized base name (see [`classify_column`]) names a
/// fixed-size scalar.
///
/// Two normalizations happen here rather than in the vocabulary:
///
/// - `unsigned`/`signed` qualifier tokens are dropped. SQLite declared types
///   are free-form, and its INTEGER-affinity family is spelled every which
///   way — `BIGINT UNSIGNED` (what a modern `mysqldump` conversion emits,
///   since MySQL 8 dropped display widths), `UNSIGNED INT`, and SQLite's own
///   documented `UNSIGNED BIG INT`. There is no principled line that admits
///   one and rejects the others.
/// - `interval` keeps its SQL field qualifiers (`interval year to month`,
///   `interval hour`, `interval day to second` — what `format_type()`
///   reports for a qualified interval), which name the same scalar type.
fn is_scalar_base(base: &str) -> bool {
    let normalized = base
        .split_whitespace()
        .filter(|word| !matches!(*word, "unsigned" | "signed"))
        .collect::<Vec<_>>()
        .join(" ");
    if normalized == "interval" || normalized.starts_with("interval ") {
        return true;
    }
    matches!(
        normalized.as_str(),
        // int/serial variants (Postgres, SQL Server, SQLite). "big int" is
        // the qualifier-stripped form of SQLite's `UNSIGNED BIG INT`.
        "int" | "integer" | "bigint" | "big int" | "smallint" | "tinyint" | "mediumint" | "int2"
            | "int4" | "int8" | "serial" | "smallserial" | "bigserial" | "serial2" | "serial4"
            | "serial8"
            // float/double/real.
            | "real" | "float" | "float4" | "float8" | "double" | "double precision"
            // numeric/decimal/money.
            | "numeric" | "decimal" | "money" | "smallmoney"
            // bool.
            | "bool" | "boolean"
            // date/time/timestamp variants.
            | "date" | "datetime" | "datetime2" | "smalldatetime" | "datetimeoffset" | "time"
            | "timetz" | "timestamp" | "timestamptz" | "time with time zone"
            | "time without time zone" | "timestamp with time zone"
            | "timestamp without time zone"
            // The rest of the fixed-size zoo. `uniqueidentifier` is SQL
            // Server's uuid: fetched whole it decodes through tiberius's
            // GUID path (hyphenated lowercase, like the query editor shows),
            // where the text preview would render T-SQL's uppercase CAST —
            // two spellings of one column, since a uuid *key* column is
            // always fetched whole anyway.
            | "uuid" | "uniqueidentifier" | "bit" | "bit varying" | "varbit" | "oid" | "point"
    )
}

/// Whether a column fetched *whole* — a key or foreign-key column, or a
/// scalar — is read through the server's own text instead of the driver's
/// decode of its wire value. The cast is the preview's ([`preview_exprs`]),
/// without the cap.
///
/// Why: a key column's value becomes the row's locator, which is bound back
/// through the column's own type to find the row again
/// (`staged::row_where`, `staged::equalities_where`). A type the backend's
/// decoder has no arm for reads as a `<typename>` marker, and no row has
/// that key: measured on PostgreSQL 17, a table keyed on `inet`, `cidr`,
/// `macaddr`, `macaddr8`, `money`, an `int4range` or `tsrange`, or
/// `tsvector` could neither load a long cell nor save an edit, and a
/// foreign-key jump from one landed nowhere. `tsvector` failed *silently*:
/// `<tsvector>` is itself a valid tsvector, so the long cell loaded as NULL
/// and the jump showed an empty page. SQL Server's `hierarchyid` read as
/// `<udt>` the same way.
///
/// - **Postgres: every text-class column, and `money`.** A type's text
///   output is what its input function reads back, which is the round trip
///   a locator needs, and it is what the same column already shows when it
///   is not a key (its preview is the same cast). That covers types no
///   decoder arm exists for — including user-defined base types — rather
///   than a list of the ones measured. It also replaces the decoder's array
///   rendering for a key or foreign key, which quotes nothing: an element
///   containing a comma read as two elements and bound back as a different
///   array. `money` is a scalar, but its binary value is a count of the
///   minor units of `lc_monetary`'s currency, which the client cannot see,
///   so only the server can print it.
///
///   The text is printed by the session that served the page and read back by
///   the one that serves the save, so it holds only while every pooled session
///   prints alike: the settings it depends on are pinned at login
///   ([`super::postgres::key_text_pins`]), and the `reg*` types, which follow
///   `search_path`, are refused as keys
///   ([`super::rowkey::key_column_addressable`]).
/// - **SQL Server: `hierarchyid` only.** Its `CAST` to `nvarchar` is
///   `ToString()` (`/1/2/`), which the server parses back on comparison.
///   SQL Server cannot have the Postgres rule: `CAST` to `nvarchar` is
///   lossy for the types an alias type can stand for (six significant
///   digits of a `float`, two decimals of `money`, minutes of a `datetime`),
///   and an alias type classifies as text by its alias name.
///   `sql_variant` is not fixed here but refused as a key
///   ([`super::rowkey::key_column_addressable`]).
/// - **SQLite: never.** Its values arrive as the five storage classes, all
///   of which decode.
pub(crate) fn fetched_as_server_text(dialect: Dialect, column: &ColumnMeta) -> bool {
    let lowered = column.type_name.trim().to_ascii_lowercase();
    match dialect {
        Dialect::Postgres => {
            lowered == "money" || classify_column(&column.type_name) == ColumnClass::Text
        }
        Dialect::SqlServer => lowered == "hierarchyid",
        Dialect::Sqlite => false,
    }
}

/// One visible column of a bounded page, and how its value was fetched. The
/// bounded fetch appends a `length` column for each previewed column; the
/// decoder uses this plan to reunite them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoundedColumn {
    pub name: String,
    pub class: ColumnClass,
    /// Whether this column was preview-wrapped (large + not an identity/FK
    /// key). When false the value is complete regardless of `class`.
    pub previewed: bool,
}

/// The decode plan produced by [`PageRequest::select_bounded_sql`]: the
/// visible columns (in SELECT order, including any prepended key column) plus
/// the count of trailing `length` helper columns to strip.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoundedPlan {
    pub columns: Vec<BoundedColumn>,
    /// Number of trailing `length(col)` columns, one per previewed column, in
    /// `columns` order.
    pub length_columns: usize,
}

/// Full-value size metadata for one previewed cell.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PreviewInfo {
    /// The underlying value's full length: characters for text/json, bytes
    /// for blobs. Always greater than [`PREVIEW_BYTES`] (only truncated cells
    /// carry a [`PreviewInfo`]).
    pub full_len: u64,
    /// Whether the underlying column is binary (blob/bytea).
    pub binary: bool,
}

/// One page fetched with bounded previews: the visible [`super::QueryResult`]
/// (large cells hold previews, not full values) plus per-cell preview
/// metadata, same shape as `result.rows`. `None` means the cell is complete.
#[derive(Debug, Clone, PartialEq)]
pub struct Page {
    pub result: super::value::QueryResult,
    pub previews: Vec<Vec<Option<PreviewInfo>>>,
}

impl Page {
    /// Whether any cell on the page is a truncated preview.
    pub fn has_truncation(&self) -> bool {
        self.previews
            .iter()
            .any(|row| row.iter().any(Option::is_some))
    }
}

impl BoundedPlan {
    /// Reunites a raw fetch — visible columns followed by this plan's trailing
    /// `length` helper columns — into a [`Page`]: strips the length columns,
    /// records which cells are truncated previews (and each one's full size),
    /// and trims any over-long text preview to [`PREVIEW_BYTES`] characters.
    pub(crate) fn assemble(&self, raw: super::value::QueryResult, preview_bytes: usize) -> Page {
        let visible = self.columns.len();
        // No previews were fetched (plain-select fallback, or a metadata
        // mismatch): every cell is complete.
        if self.length_columns == 0 {
            let previews = raw.rows.iter().map(|row| vec![None; row.len()]).collect();
            return Page {
                result: raw,
                previews,
            };
        }
        // Visible column indexes of the previewed columns, in the same order
        // their `length` columns were appended.
        let previewed: Vec<(usize, ColumnClass)> = self
            .columns
            .iter()
            .enumerate()
            .filter(|(_, c)| c.previewed)
            .map(|(i, c)| (i, c.class))
            .collect();

        let mut out_rows = Vec::with_capacity(raw.rows.len());
        let mut out_previews = Vec::with_capacity(raw.rows.len());
        for mut row in raw.rows {
            // Split off the trailing length columns.
            let lengths: Vec<Value> = row.split_off(visible.min(row.len()));
            let mut cell_previews = vec![None; row.len()];
            for (slot, &(vi, class)) in lengths.iter().zip(previewed.iter()) {
                let Value::Integer(full) = slot else {
                    continue; // NULL length ⇒ NULL cell ⇒ nothing to preview.
                };
                let full_len = *full as u64;
                if full_len as usize <= preview_bytes {
                    continue; // Complete value (blob keeps its real bytes/size).
                }
                if vi >= row.len() {
                    continue;
                }
                let binary = matches!(class, ColumnClass::Binary);
                // Belt-and-braces: a text preview should already be ≤ N from
                // substr/left, but trim to be certain memory is bounded.
                if let Value::Text(t) = &mut row[vi] {
                    if t.chars().count() > preview_bytes {
                        *t = t.chars().take(preview_bytes).collect();
                    }
                }
                cell_previews[vi] = Some(PreviewInfo { full_len, binary });
            }
            out_rows.push(row);
            out_previews.push(cell_previews);
        }
        let mut columns = raw.columns;
        columns.truncate(visible);
        Page {
            result: super::value::QueryResult {
                columns,
                rows: out_rows,
            },
            previews: out_previews,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortDir {
    Asc,
    Desc,
}

impl SortDir {
    fn sql(self) -> &'static str {
        match self {
            SortDir::Asc => "ASC",
            SortDir::Desc => "DESC",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FilterOp {
    Contains,
    Equals,
}

/// The active grid filter. Two shapes share one WHERE-clause builder:
///
/// - [`Filter::Column`] is the manual filter bar — one column compared with
///   `contains`/`equals` against a text value the user typed.
/// - [`Filter::Equalities`] is a conjunction of `column = value` equalities,
///   installed by foreign-key navigation to pin the referenced row
///   across one or more columns. Values come from the source row, so they are
///   already typed [`Value`]s rather than user text, and they are compared
///   as typed values — the predicate a row's own key is re-addressed by —
///   not as text the way the filter bar compares.
///
/// (No `Eq`: [`Value::Real`] holds an `f64`. Nothing keys a map on a filter.)
#[derive(Debug, Clone, PartialEq)]
pub enum Filter {
    Column {
        column: String,
        op: FilterOp,
        value: String,
    },
    /// AND'd `column = value` equalities. An empty list matches every row
    /// (no WHERE clause) — callers never build one.
    Equalities(Vec<KeyEquality>),
}

/// One `column = value` term of [`Filter::Equalities`]: a typed value
/// compared against a column of the filtered table, bound by that column's
/// type exactly as a key value is when a row is re-addressed for a read or a
/// write.
///
/// The column's metadata is resolved when the filter is built
/// ([`super::build_fk_filter`] reads it off the target table), so every
/// consumer of the filter — the page, its row count, an export — renders the
/// same predicate from the filter alone, with nothing of its own to supply.
#[derive(Debug, Clone, PartialEq)]
pub struct KeyEquality {
    pub column: String,
    pub value: Value,
    /// The compared column's introspected metadata, which decides how the
    /// value binds: its cast (every plain Postgres type, a SQL Server `real`)
    /// and the spelling `staged` adjusts for the server (a SQL Server
    /// `datetime`). `None`
    /// binds the value as it is.
    pub target: Option<ColumnMeta>,
}

impl KeyEquality {
    /// A term with no column metadata — bound as the value is.
    pub fn untyped(column: impl Into<String>, value: Value) -> KeyEquality {
        KeyEquality {
            column: column.into(),
            value,
            target: None,
        }
    }
}

impl Filter {
    /// A single-column `contains` filter (the filter bar's default).
    pub fn contains(column: impl Into<String>, value: impl Into<String>) -> Filter {
        Filter::Column {
            column: column.into(),
            op: FilterOp::Contains,
            value: value.into(),
        }
    }

    /// A single-column `equals` filter.
    pub fn equals(column: impl Into<String>, value: impl Into<String>) -> Filter {
        Filter::Column {
            column: column.into(),
            op: FilterOp::Equals,
            value: value.into(),
        }
    }
}

/// One page of one table, with optional sort and filter.
#[derive(Debug, Clone, PartialEq)]
pub struct PageRequest {
    /// Schema qualifier (`None` for SQLite / default resolution).
    pub schema: Option<String>,
    pub table: String,
    pub limit: u32,
    pub offset: u64,
    pub sort: Option<(String, SortDir)>,
    pub filter: Option<Filter>,
    /// Extra column selected ahead of `*`, e.g. `SELECT "rowid", * FROM …`.
    /// Used for SQLite tables whose row identity is the implicit rowid
    /// ([`RowIdentity::Rowid`](super::rowkey::RowIdentity::Rowid)): the
    /// rowid is not part of `*`, but staged edits need it in every fetched
    /// row to build row locators. The grid hides the column from display.
    pub extra_key_column: Option<String>,
}

impl PageRequest {
    /// SELECT for this page: SQL plus bound parameters.
    pub fn select_sql(&self, dialect: Dialect) -> (String, Vec<Value>) {
        let (where_clause, params) = self.where_clause(dialect);
        let extra = match &self.extra_key_column {
            Some(column) => format!("{}, ", quote_ident(column)),
            None => String::new(),
        };
        let sql = format!(
            "SELECT {extra}* FROM {}{where_clause}{}{}",
            self.qualified_table(),
            self.order_clause(),
            self.limit_offset(dialect),
        );
        (sql, params)
    }

    /// The ` ORDER BY …` clause for the active sort (empty when unsorted).
    fn order_clause(&self) -> String {
        match &self.sort {
            Some((column, dir)) => format!(" ORDER BY {} {}", quote_ident(column), dir.sql()),
            None => String::new(),
        }
    }

    /// The paging tail shared by the paged selects, appended right after
    /// [`Self::order_clause`]. SQLite and Postgres share the
    /// ` LIMIT n OFFSET m` shape. SQL Server pages with
    /// ` OFFSET n ROWS FETCH NEXT m ROWS ONLY`, which is only valid after an
    /// ORDER BY — so when the request is unsorted (no order clause was
    /// emitted) the tail supplies a synthetic ` ORDER BY (SELECT NULL)`
    /// first, keeping the SQL valid without imposing an actual order.
    fn limit_offset(&self, dialect: Dialect) -> String {
        match dialect {
            Dialect::Sqlite | Dialect::Postgres => {
                format!(" LIMIT {} OFFSET {}", self.limit, self.offset)
            }
            Dialect::SqlServer => {
                let order = if self.sort.is_some() {
                    ""
                } else {
                    " ORDER BY (SELECT NULL)"
                };
                format!(
                    "{order} OFFSET {} ROWS FETCH NEXT {} ROWS ONLY",
                    self.offset, self.limit
                )
            }
        }
    }

    /// SELECT for this page that fetches a bounded preview of large columns
    /// (long text / json / blobs) instead of their full contents, so peak
    /// memory does not scale with value size. Returns the SQL, its
    /// bound parameters, and the [`BoundedPlan`] the decoder needs to strip
    /// the trailing `length` helper columns and rebuild the visible result.
    ///
    /// `columns` are the table's visible columns in `*` order (from
    /// introspection); `no_preview` names columns that must always be fetched
    /// whole (identity keys and foreign-key columns — a truncated key would
    /// misaddress rows or misdirect a foreign-key jump) — through the
    /// server's text where [`fetched_as_server_text`] says the driver's
    /// decode of them could not be bound back. Falls back to the plain
    /// [`Self::select_sql`] (empty plan) when nothing qualifies for a preview
    /// or that text, so callers without metadata behave exactly as before.
    pub fn select_bounded_sql(
        &self,
        dialect: Dialect,
        columns: &[ColumnMeta],
        no_preview: &[&str],
        preview_bytes: usize,
    ) -> (String, Vec<Value>, BoundedPlan) {
        // Decide each column's fetch treatment.
        let mut specs: Vec<BoundedColumn> = Vec::new();
        if let Some(key) = &self.extra_key_column {
            // The prepended identity key column is always a scalar fetched
            // whole (it feeds row locators).
            specs.push(BoundedColumn {
                name: key.clone(),
                class: ColumnClass::Scalar,
                previewed: false,
            });
        }
        for column in columns {
            let class = classify_column(&column.type_name);
            let previewed = matches!(class, ColumnClass::Text | ColumnClass::Binary)
                && !no_preview.contains(&column.name.as_str());
            specs.push(BoundedColumn {
                name: column.name.clone(),
                class,
                previewed,
            });
        }
        // Columns fetched whole but through the server's text, by name. A
        // previewed column is read through the text already; it is left out
        // because the sort below must not name it (see there).
        let as_text: Vec<&str> = columns
            .iter()
            .filter(|column| fetched_as_server_text(dialect, column))
            .map(|column| column.name.as_str())
            .filter(|name| specs.iter().any(|s| s.name == *name && !s.previewed))
            .collect();
        // Nothing large to bound and nothing to read as text → identical to
        // the plain page select.
        if columns.is_empty() || (!specs.iter().any(|s| s.previewed) && as_text.is_empty()) {
            let (sql, params) = self.select_sql(dialect);
            return (
                sql,
                params,
                BoundedPlan {
                    columns: specs,
                    length_columns: 0,
                },
            );
        }
        let n = preview_bytes as i64;
        let mut value_exprs: Vec<String> = Vec::with_capacity(specs.len());
        let mut length_exprs: Vec<String> = Vec::new();
        for spec in &specs {
            let q = quote_ident(&spec.name);
            if !spec.previewed {
                if as_text.contains(&spec.name.as_str()) {
                    value_exprs.push(format!("{} AS {q}", dialect.cast_expr(&q, "text")));
                } else {
                    value_exprs.push(q);
                }
                continue;
            }
            // Previewed columns are Text or Binary by construction (scalars
            // are never previewed), so the pair always carries a real length
            // probe. The expression shapes are shared with the on-demand
            // cell fetch — see [`preview_exprs`].
            let (value_expr, length_expr) = preview_exprs(dialect, spec.class, &q, n);
            value_exprs.push(format!("{value_expr} AS {q}"));
            length_exprs.push(length_expr);
        }
        let length_columns = length_exprs.len();
        let mut select_list = value_exprs.join(", ");
        if !length_exprs.is_empty() {
            select_list.push_str(", ");
            select_list.push_str(&length_exprs.join(", "));
        }
        let (where_clause, params) = self.where_clause(dialect);
        // A sort on a column read whole through its text names the table's
        // column, not the cast: both engines resolve a bare `ORDER BY "c"` to
        // the output column `… AS "c"` first, which would sort an `int4range`
        // key `[10,20)` before `[9,10)` and a `hierarchyid` `/10/` before
        // `/2/`. Only those columns: they are keys and foreign keys (plus
        // Postgres `money`), whose types always have an ordering. A previewed
        // column keeps sorting by its preview's text, as it always has —
        // naming the column there was measured to turn a sort on a `json`
        // column (PostgreSQL) or a `tsvector` one (CockroachDB) into an error,
        // since neither type can be ordered.
        let order = match &self.sort {
            Some((column, dir)) if as_text.contains(&column.as_str()) => format!(
                " ORDER BY {}.{} {}",
                self.qualified_table(),
                quote_ident(column),
                dir.sql()
            ),
            _ => self.order_clause(),
        };
        let sql = format!(
            "SELECT {select_list} FROM {}{where_clause}{order}{}",
            self.qualified_table(),
            self.limit_offset(dialect),
        );
        (
            sql,
            params,
            BoundedPlan {
                columns: specs,
                length_columns,
            },
        )
    }

    /// SELECT for exporting the current view: the same table, filter, and
    /// sort as [`Self::select_sql`], but WITHOUT `LIMIT`/`OFFSET` (every
    /// matching row) and without the hidden `extra_key_column` (exports show
    /// the visible `*` columns only). Row order still follows the active
    /// sort so a sorted export matches what the grid shows.
    pub fn export_sql(&self, dialect: Dialect) -> (String, Vec<Value>) {
        let (where_clause, params) = self.where_clause(dialect);
        let sql = format!(
            "SELECT * FROM {}{where_clause}{}",
            self.qualified_table(),
            self.order_clause(),
        );
        (sql, params)
    }

    /// COUNT(*) with the same filter, for the row-count indicator.
    pub fn count_sql(&self, dialect: Dialect) -> (String, Vec<Value>) {
        let (where_clause, params) = self.where_clause(dialect);
        let sql = format!(
            "SELECT COUNT(*) FROM {}{where_clause}",
            self.qualified_table()
        );
        (sql, params)
    }

    /// Thin wrapper over [`super::sql::qualified`] for this request's table.
    fn qualified_table(&self) -> String {
        qualified(self.schema.as_deref(), &self.table)
    }

    fn where_clause(&self, dialect: Dialect) -> (String, Vec<Value>) {
        match &self.filter {
            None => (String::new(), Vec::new()),
            Some(Filter::Column { column, op, value }) => {
                // Filters compare as text — see [`text_compare_expr`] for the
                // strategy (Postgres/SQL Server cast the column to text;
                // SQLite affinity coerces on its own).
                let quoted = text_compare_expr(dialect, column);
                let placeholder = dialect.placeholder(1);
                match op {
                    FilterOp::Equals => (
                        format!(" WHERE {quoted} = {placeholder}"),
                        vec![Value::Text(value.clone())],
                    ),
                    FilterOp::Contains => (
                        format!(" WHERE {quoted} LIKE {placeholder} ESCAPE '\\'"),
                        vec![Value::Text(format!("%{}%", escape_like(value, dialect)))],
                    ),
                }
            }
            // Typed, never as text: see [`equalities_where`].
            Some(Filter::Equalities(terms)) => equalities_where(terms, dialect),
        }
    }
}

/// Escapes LIKE wildcards in user input so "50%" matches literally, for the
/// `ESCAPE '\\'` clause the contains filter is built with.
///
/// SQL Server's `LIKE` has a third wildcard the standard lacks: `[…]` is a
/// character class, so `[a]` matched every value containing an `a`, and a lone
/// `[` opened a class that never closed and matched nothing. Escaping the
/// opening bracket is enough — measured on SQL Server 2022, `]` and `^` are
/// literal outside a class, and `\[a]` matches only the text `[a]`. Postgres
/// and SQLite have no bracket syntax and are left alone: both read `\[` as a
/// literal `[` too, but nothing there needs it.
fn escape_like(needle: &str, dialect: Dialect) -> String {
    let escaped = needle
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_");
    match dialect {
        Dialect::SqlServer => escaped.replace('[', "\\["),
        Dialect::Sqlite | Dialect::Postgres => escaped,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> PageRequest {
        PageRequest {
            schema: None,
            table: "tracks".into(),
            limit: 100,
            offset: 200,
            sort: None,
            filter: None,
            extra_key_column: None,
        }
    }

    #[test]
    fn plain_page_selects_with_limit_offset() {
        let (sql, params) = base().select_sql(Dialect::Sqlite);
        assert_eq!(sql, "SELECT * FROM \"tracks\" LIMIT 100 OFFSET 200");
        assert!(params.is_empty());
    }

    #[test]
    fn sort_adds_order_by_with_quoted_column() {
        let mut req = base();
        req.sort = Some(("na\"me".into(), SortDir::Desc));
        let (sql, _) = req.select_sql(Dialect::Sqlite);
        assert_eq!(
            sql,
            "SELECT * FROM \"tracks\" ORDER BY \"na\"\"me\" DESC LIMIT 100 OFFSET 200"
        );
    }

    #[test]
    fn equals_filter_binds_the_value() {
        let mut req = base();
        req.filter = Some(Filter::equals("name", "Track 7"));
        let (sql, params) = req.select_sql(Dialect::Sqlite);
        assert_eq!(
            sql,
            "SELECT * FROM \"tracks\" WHERE \"name\" = ? LIMIT 100 OFFSET 200"
        );
        assert_eq!(params, vec![Value::Text("Track 7".into())]);
    }

    #[test]
    fn contains_filter_escapes_like_wildcards() {
        let mut req = base();
        req.filter = Some(Filter::contains("name", "50%_\\"));
        let (sql, params) = req.select_sql(Dialect::Sqlite);
        assert_eq!(
            sql,
            "SELECT * FROM \"tracks\" WHERE \"name\" LIKE ? ESCAPE '\\' LIMIT 100 OFFSET 200"
        );
        assert_eq!(params, vec![Value::Text("%50\\%\\_\\\\%".into())]);
    }

    #[test]
    fn extra_key_column_is_selected_ahead_of_star() {
        let mut req = base();
        req.extra_key_column = Some("rowid".into());
        let (sql, params) = req.select_sql(Dialect::Sqlite);
        assert_eq!(
            sql,
            "SELECT \"rowid\", * FROM \"tracks\" LIMIT 100 OFFSET 200"
        );
        assert!(params.is_empty());
        // COUNT(*) is unaffected.
        let (count_sql, _) = req.count_sql(Dialect::Sqlite);
        assert_eq!(count_sql, "SELECT COUNT(*) FROM \"tracks\"");
    }

    #[test]
    fn export_sql_drops_limit_offset_and_extra_key_but_keeps_filter_and_sort() {
        let mut req = base();
        req.extra_key_column = Some("rowid".into());
        req.sort = Some(("name".into(), SortDir::Desc));
        req.filter = Some(Filter::contains("name", "abc"));
        let (sql, params) = req.export_sql(Dialect::Sqlite);
        assert_eq!(
            sql,
            "SELECT * FROM \"tracks\" WHERE \"name\" LIKE ? ESCAPE '\\' ORDER BY \"name\" DESC"
        );
        assert_eq!(params, vec![Value::Text("%abc%".into())]);
    }

    #[test]
    fn export_sql_plain_is_select_star() {
        let (sql, params) = base().export_sql(Dialect::Postgres);
        assert_eq!(sql, "SELECT * FROM \"tracks\"");
        assert!(params.is_empty());
    }

    #[test]
    fn schema_qualifier_is_quoted_when_present() {
        let mut req = base();
        req.schema = Some("app data".into());
        let (sql, _) = req.select_sql(Dialect::Postgres);
        assert_eq!(
            sql,
            "SELECT * FROM \"app data\".\"tracks\" LIMIT 100 OFFSET 200"
        );
    }

    #[test]
    fn postgres_dialect_uses_dollar_placeholder_and_text_cast() {
        let mut req = base();
        req.filter = Some(Filter::equals("name", "7"));
        let (sql, params) = req.select_sql(Dialect::Postgres);
        assert_eq!(
            sql,
            "SELECT * FROM \"tracks\" WHERE \"name\"::text = $1 LIMIT 100 OFFSET 200"
        );
        assert_eq!(params, vec![Value::Text("7".into())]);
        let (count_sql, _) = req.count_sql(Dialect::Postgres);
        assert_eq!(
            count_sql,
            "SELECT COUNT(*) FROM \"tracks\" WHERE \"name\"::text = $1"
        );
    }

    #[test]
    fn sqlserver_placeholders_are_numbered_at_p() {
        assert_eq!(Dialect::SqlServer.placeholder(1), "@P1");
        assert_eq!(Dialect::SqlServer.placeholder(12), "@P12");
        assert_eq!(Dialect::Sqlite.placeholder(3), "?");
        assert_eq!(Dialect::Postgres.placeholder(3), "$3");
    }

    #[test]
    fn cast_expr_is_postfix_on_postgres_and_prefix_on_sqlserver() {
        assert_eq!(Dialect::Postgres.cast_expr("\"c\"", "text"), "\"c\"::text");
        assert_eq!(Dialect::Postgres.cast_expr("$1", "integer"), "$1::integer");
        // The dialect-neutral "text" stringify target maps to nvarchar(max).
        assert_eq!(
            Dialect::SqlServer.cast_expr("\"c\"", "text"),
            "CAST(\"c\" AS nvarchar(max))"
        );
        // Any other target renders verbatim.
        assert_eq!(
            Dialect::SqlServer.cast_expr("@P1", "int"),
            "CAST(@P1 AS int)"
        );
    }

    #[test]
    fn sqlserver_unsorted_page_gets_a_synthetic_order_by() {
        // OFFSET/FETCH is only valid after an ORDER BY; unsorted requests
        // must emit ORDER BY (SELECT NULL), never invalid SQL.
        let (sql, params) = base().select_sql(Dialect::SqlServer);
        assert_eq!(
            sql,
            "SELECT * FROM \"tracks\" ORDER BY (SELECT NULL) \
             OFFSET 200 ROWS FETCH NEXT 100 ROWS ONLY"
        );
        assert!(params.is_empty());
    }

    #[test]
    fn sqlserver_sorted_page_uses_the_real_order_by() {
        let mut req = base();
        req.sort = Some(("name".into(), SortDir::Desc));
        let (sql, _) = req.select_sql(Dialect::SqlServer);
        assert_eq!(
            sql,
            "SELECT * FROM \"tracks\" ORDER BY \"name\" DESC \
             OFFSET 200 ROWS FETCH NEXT 100 ROWS ONLY"
        );
    }

    #[test]
    fn sqlserver_filter_casts_and_numbers_placeholders() {
        let mut req = base();
        req.filter = Some(Filter::equals("name", "7"));
        let (sql, params) = req.select_sql(Dialect::SqlServer);
        assert_eq!(
            sql,
            "SELECT * FROM \"tracks\" WHERE CAST(\"name\" AS nvarchar(max)) = @P1 \
             ORDER BY (SELECT NULL) OFFSET 200 ROWS FETCH NEXT 100 ROWS ONLY"
        );
        assert_eq!(params, vec![Value::Text("7".into())]);
        // COUNT(*) shares the filter but never pages (no ORDER BY needed).
        let (count_sql, _) = req.count_sql(Dialect::SqlServer);
        assert_eq!(
            count_sql,
            "SELECT COUNT(*) FROM \"tracks\" WHERE CAST(\"name\" AS nvarchar(max)) = @P1"
        );
    }

    #[test]
    fn contains_escapes_sql_server_brackets_and_only_there() {
        // T-SQL reads `[…]` as a character class, so `[a]` would match every
        // value holding an `a`. The opening bracket is escaped with the
        // clause's own escape character, after the backslash itself is.
        let mut req = base();
        req.filter = Some(Filter::contains("name", "x[a]\\%_["));
        let (sql, params) = req.count_sql(Dialect::SqlServer);
        assert_eq!(
            sql,
            "SELECT COUNT(*) FROM \"tracks\" \
             WHERE CAST(\"name\" AS nvarchar(max)) LIKE @P1 ESCAPE '\\'"
        );
        assert_eq!(params, vec![Value::Text("%x\\[a]\\\\\\%\\_\\[%".into())]);
        // Postgres and SQLite have no bracket wildcard: `[` stays as typed.
        for dialect in [Dialect::Postgres, Dialect::Sqlite] {
            let (_, params) = req.count_sql(dialect);
            assert_eq!(
                params,
                vec![Value::Text("%x[a]\\\\\\%\\_[%".into())],
                "{dialect:?}"
            );
        }
    }

    #[test]
    fn sqlserver_equalities_filter_binds_typed_values_and_numbers_placeholders() {
        // A jump's values are the source row's typed cells: they bind as what
        // they are, never as hubro's text spelling against the column cast
        // to `nvarchar` — `CAST(float AS nvarchar)` keeps six digits.
        let mut req = base();
        req.filter = Some(Filter::Equalities(vec![
            KeyEquality::untyped("region", Value::Text("eu".into())),
            KeyEquality::untyped("ratio", Value::Real(1e20)),
        ]));
        let (sql, params) = req.select_sql(Dialect::SqlServer);
        assert_eq!(
            sql,
            "SELECT * FROM \"tracks\" WHERE \"region\" = @P1 AND \"ratio\" = @P2 \
             ORDER BY (SELECT NULL) OFFSET 200 ROWS FETCH NEXT 100 ROWS ONLY"
        );
        assert_eq!(params, vec![Value::Text("eu".into()), Value::Real(1e20)]);
    }

    #[test]
    fn sqlserver_export_has_no_paging_and_no_synthetic_order() {
        let mut req = base();
        req.filter = Some(Filter::equals("name", "x"));
        let (sql, _) = req.export_sql(Dialect::SqlServer);
        assert_eq!(
            sql,
            "SELECT * FROM \"tracks\" WHERE CAST(\"name\" AS nvarchar(max)) = @P1"
        );
    }

    #[test]
    fn count_shares_the_filter_but_not_paging() {
        let mut req = base();
        req.filter = Some(Filter::contains("name", "abc"));
        let (sql, params) = req.count_sql(Dialect::Sqlite);
        assert_eq!(
            sql,
            "SELECT COUNT(*) FROM \"tracks\" WHERE \"name\" LIKE ? ESCAPE '\\'"
        );
        assert_eq!(params.len(), 1);
    }

    #[test]
    fn equalities_filter_ands_the_pairs_sqlite() {
        let mut req = base();
        req.filter = Some(Filter::Equalities(vec![
            KeyEquality::untyped("artist_id", Value::Integer(1)),
            KeyEquality::untyped("blob_key", Value::Blob(vec![0, 255])),
        ]));
        let (sql, params) = req.select_sql(Dialect::Sqlite);
        assert_eq!(
            sql,
            "SELECT * FROM \"tracks\" WHERE \"artist_id\" = ? AND \"blob_key\" = ? \
             LIMIT 100 OFFSET 200"
        );
        // Values bind as what they are: a blob as bytes, not the display
        // marker `<blob 2>` the text comparison used to bind.
        assert_eq!(params, vec![Value::Integer(1), Value::Blob(vec![0, 255])]);
    }

    #[test]
    fn equalities_filter_casts_the_bind_not_the_column_on_postgres() {
        let mut req = base();
        req.filter = Some(Filter::Equalities(vec![
            KeyEquality {
                column: "region".into(),
                value: Value::Text("eu ".into()),
                target: Some(col("region", "character", None)),
            },
            KeyEquality {
                column: "ratio".into(),
                value: Value::Real(1e20),
                target: Some(col("ratio", "double precision", None)),
            },
        ]));
        let (sql, params) = req.select_sql(Dialect::Postgres);
        let clause = " WHERE \"region\" = $1::bpchar AND \"ratio\" = $2::double precision";
        assert_eq!(
            sql,
            format!("SELECT * FROM \"tracks\"{clause} LIMIT 100 OFFSET 200")
        );
        // Under a cast every value binds as text the cast parses, so one
        // statement has one bind shape whatever the source column was.
        assert_eq!(
            params,
            vec![Value::Text("eu ".into()), Value::Text("1e20".into())]
        );
        // The count and the export share the same clause and binds — every
        // consumer of the jump renders the one predicate.
        let (count_sql, count_params) = req.count_sql(Dialect::Postgres);
        assert_eq!(
            count_sql,
            format!("SELECT COUNT(*) FROM \"tracks\"{clause}")
        );
        assert_eq!(count_params, params);
        let (export_sql, export_params) = req.export_sql(Dialect::Postgres);
        assert_eq!(export_sql, format!("SELECT * FROM \"tracks\"{clause}"));
        assert_eq!(export_params, params);
    }

    #[test]
    fn equalities_filter_quotes_weird_identifiers() {
        let mut req = base();
        req.filter = Some(Filter::Equalities(vec![KeyEquality::untyped(
            "col\"name",
            Value::Text("x".into()),
        )]));
        let (sql, _) = req.select_sql(Dialect::Sqlite);
        assert_eq!(
            sql,
            "SELECT * FROM \"tracks\" WHERE \"col\"\"name\" = ? LIMIT 100 OFFSET 200"
        );
    }

    #[test]
    fn empty_equalities_filter_matches_everything() {
        let mut req = base();
        req.filter = Some(Filter::Equalities(vec![]));
        let (sql, params) = req.select_sql(Dialect::Sqlite);
        assert_eq!(sql, "SELECT * FROM \"tracks\" LIMIT 100 OFFSET 200");
        assert!(params.is_empty());
    }

    // ---- Bounded previews ------------------------------------

    use crate::db::schema::Generated;
    use crate::db::value::{ColumnInfo, QueryResult};

    fn col(name: &str, type_name: &str, pk: Option<u32>) -> ColumnMeta {
        ColumnMeta {
            name: name.into(),
            type_name: type_name.into(),
            nullable: true,
            primary_key_position: pk,
            default: None,
            generated: Generated::Never,
            type_detail: crate::db::TypeDetail::Plain,
        }
    }

    #[test]
    fn classify_column_separates_scalars_text_and_binary() {
        use ColumnClass::*;
        // Binary. SQL Server types match on the exact base name, with any
        // parameter suffix stripped and case ignored.
        for t in [
            "BLOB",
            "bytea",
            "BLOB SUB_TYPE TEXT",
            "varbinary(max)",
            "VARBINARY(255)",
            "binary(16)",
            "BINARY(16)",
            "image",
            "varbinary (max)",
        ] {
            assert_eq!(classify_column(t), Binary, "{t}");
        }
        // Postgres user-defined types (enums/domains) that merely CONTAIN a
        // binary-ish word are NOT binary — byte-preview SQL would error on
        // them. They fall through to Text like any user-defined type.
        for t in ["image_format", "binary_state", "imagery", "combinary"] {
            assert_eq!(classify_column(t), Text, "{t}");
        }
        // Scalars keep their native decoded type (never previewed).
        for t in [
            "INTEGER",
            "bigint",
            "real",
            "double precision",
            "numeric(10,2)",
            "boolean",
            "timestamp without time zone",
            "date",
            "uuid",
            "bigserial",
            "point",
        ] {
            assert_eq!(classify_column(t), Scalar, "{t}");
        }
        // Text-like and unknown/empty → previewed.
        for t in [
            "TEXT",
            "VARCHAR(40)",
            "character varying",
            "clob",
            "json",
            "jsonb",
            "xml",
            "",
            "USER-DEFINED",
        ] {
            assert_eq!(classify_column(t), Text, "{t}");
        }
        // SQL Server sql_variant and CLR UDT columns MUST stay Text (the
        // unknown-type fallback): selecting them raw panics inside the
        // tiberius codec, while the Text class routes them
        // through the `CAST(… AS nvarchar(max))` preview, which SQL Server
        // accepts for all of them (CLR UDTs stringify via their ToString).
        for t in ["sql_variant", "hierarchyid", "geography", "geometry"] {
            assert_eq!(classify_column(t), Text, "{t}");
        }
    }

    #[test]
    fn classify_column_matches_scalars_by_exact_base_name_not_substring() {
        use ColumnClass::*;
        // A type merely CONTAINING a scalar word must stay Text (previewed):
        // a Scalar classification would fetch it whole and let an
        // arbitrarily large value escape the preview memory bound. These
        // reach `classify_column` as matview `format_type()` output, SQL
        // Server alias types, and SQLite declared types.
        for t in [
            "fingerprint_data", // contains "int"
            "app.timeline",     // contains "time" — matview format_type
            "app.fingerprint",  // contains "int"
            "validated",        // contains "date"
            "serialized",       // contains "serial"
            "orbit",            // contains "bit"
            "unreal",           // contains "real"
            "interval_stats",   // starts with "interval", but is one word
            "booleanish",       // contains "boolean"
            "pointer",          // contains "point"
            "moneybag",         // contains "money"
        ] {
            assert_eq!(classify_column(t), Text, "{t}");
        }
        // Genuine scalar names still match: exact base name, any parameter
        // suffix stripped, case ignored, multi-word names included.
        for t in [
            "int",
            "tinyint",
            "int8",
            "smallserial",
            "float4",
            "decimal(18,2)",
            "NUMERIC(10, 2)",
            "smallmoney",
            "datetime2(7)",
            "datetimeoffset",
            "timestamp(3) with time zone",
            "time without time zone",
            "bit",
            "bit varying(8)",
        ] {
            assert_eq!(classify_column(t), Scalar, "{t}");
        }
        // SQLite declared types are free-form, and its INTEGER-affinity
        // family spells the unsigned qualifier every which way. All of them
        // are the same scalar; the qualifier is normalized away.
        for t in [
            "UNSIGNED BIG INT", // SQLite's own documented spelling
            "bigint unsigned",  // what a modern mysqldump conversion emits
            "BIGINT UNSIGNED",
            "int unsigned",
            "integer unsigned",
            "smallint unsigned",
            "tinyint unsigned",
            "unsigned int",
            "unsigned integer",
            "int(11) unsigned",
            "decimal(10,2) unsigned",
        ] {
            assert_eq!(classify_column(t), Scalar, "{t}");
        }
        // An interval keeps its field qualifiers (matview `format_type()`
        // output) and is still the same scalar type.
        for t in [
            "interval",
            "interval year to month",
            "interval hour",
            "interval day to second(3)",
        ] {
            assert_eq!(classify_column(t), Scalar, "{t}");
        }
        // SQL Server's uuid spelling is fetched whole like `uuid`, so a
        // GUID column reads the same whether or not it is a key column.
        assert_eq!(classify_column("uniqueidentifier"), Scalar);
    }

    #[test]
    fn classify_column_previews_ranges_and_arrays_uniformly() {
        use ColumnClass::*;
        // Postgres range/multirange types all preview — deliberately one
        // rule, where the old substring vocabulary split them by accident
        // (`int4range` matched "int", `numrange` matched nothing).
        for t in [
            "int4range",
            "int8range",
            "numrange",
            "daterange",
            "tsrange",
            "tstzrange",
            "int4multirange",
            "datemultirange",
        ] {
            assert_eq!(classify_column(t), Text, "{t}");
        }
        // Arrays preview under BOTH introspection spellings: `ARRAY` from an
        // ordinary table's `data_type`, and `format_type()`'s element form
        // from a materialized view. The two paths agree.
        for t in [
            "ARRAY",
            "integer[]",
            "bigint[]",
            "uuid[]",
            "timestamp with time zone[]",
        ] {
            assert_eq!(classify_column(t), Text, "{t}");
        }
    }

    #[test]
    fn bounded_select_wraps_only_large_non_key_columns_sqlite() {
        let mut req = base();
        req.extra_key_column = Some("rowid".into());
        let columns = [
            col("id", "INTEGER", Some(1)),
            col("body", "TEXT", None),
            col("cover", "BLOB", None),
            col("tag", "VARCHAR(8)", None),
        ];
        // `id` is a key column → fetched whole even though nothing forbids it;
        // the scalar rule already skips it, but pass it as a key to be sure.
        let (sql, params, plan) = req.select_bounded_sql(Dialect::Sqlite, &columns, &["id"], 2048);
        assert_eq!(
            sql,
            "SELECT \"rowid\", \"id\", substr(\"body\", 1, 2048) AS \"body\", \
             substr(\"cover\", 1, 2048) AS \"cover\", substr(\"tag\", 1, 2048) AS \"tag\", \
             length(\"body\"), length(\"cover\"), length(\"tag\") \
             FROM \"tracks\" LIMIT 100 OFFSET 200"
        );
        assert!(params.is_empty());
        assert_eq!(plan.length_columns, 3);
        // The prepended rowid and the scalar id are not previewed.
        assert!(!plan.columns[0].previewed && plan.columns[0].name == "rowid");
        assert!(!plan.columns[1].previewed && plan.columns[1].name == "id");
        assert!(plan.columns[2].previewed && plan.columns[2].class == ColumnClass::Text);
        assert!(plan.columns[3].previewed && plan.columns[3].class == ColumnClass::Binary);
    }

    #[test]
    fn bounded_select_uses_left_and_octet_length_on_postgres() {
        let req = base();
        let columns = [
            col("id", "integer", Some(1)),
            col("body", "text", None),
            col("blob", "bytea", None),
        ];
        let (sql, _params, plan) =
            req.select_bounded_sql(Dialect::Postgres, &columns, &["id"], 512);
        assert_eq!(
            sql,
            "SELECT \"id\", left(\"body\"::text, 512) AS \"body\", \
             substring(\"blob\" from 1 for 512) AS \"blob\", \
             length(\"body\"::text), octet_length(\"blob\") \
             FROM \"tracks\" LIMIT 100 OFFSET 200"
        );
        assert_eq!(plan.length_columns, 2);
    }

    #[test]
    fn bounded_select_uses_substring_len_and_datalength_on_sqlserver() {
        let req = base();
        let columns = [
            col("id", "int", Some(1)),
            col("body", "nvarchar(max)", None),
            col("blob", "varbinary(max)", None),
        ];
        let (sql, _params, plan) =
            req.select_bounded_sql(Dialect::SqlServer, &columns, &["id"], 512);
        assert_eq!(
            sql,
            "SELECT \"id\", SUBSTRING(CAST(\"body\" AS nvarchar(max)), 1, 512) AS \"body\", \
             SUBSTRING(\"blob\", 1, 512) AS \"blob\", \
             DATALENGTH(CAST(\"body\" AS nvarchar(max))) / 2, DATALENGTH(\"blob\") \
             FROM \"tracks\" ORDER BY (SELECT NULL) OFFSET 200 ROWS FETCH NEXT 100 ROWS ONLY"
        );
        assert_eq!(plan.length_columns, 2);
    }

    #[test]
    fn bounded_select_falls_back_to_plain_when_nothing_is_large() {
        let req = base();
        let columns = [col("id", "INTEGER", Some(1)), col("n", "REAL", None)];
        let (sql, _params, plan) = req.select_bounded_sql(Dialect::Sqlite, &columns, &["id"], 2048);
        assert_eq!(sql, "SELECT * FROM \"tracks\" LIMIT 100 OFFSET 200");
        assert_eq!(plan.length_columns, 0);
        assert!(plan.columns.iter().all(|c| !c.previewed));
    }

    #[test]
    fn postgres_keys_without_a_scalar_decode_are_read_as_the_servers_text() {
        // A key or foreign key the class would preview is fetched whole as
        // its `::text` — the same text its preview shows when it is not a
        // key — and so is `money`, whose binary value only the server can
        // print. Every other scalar is fetched as itself.
        let columns = [
            col("addr", "inet", Some(1)),
            col("span", "int4range", None),
            col("price", "money", None),
            col("n", "integer", None),
            col("oid", "oid", None),
        ];
        let (sql, _params, plan) =
            base().select_bounded_sql(Dialect::Postgres, &columns, &["addr", "span"], 512);
        assert_eq!(
            sql,
            "SELECT \"addr\"::text AS \"addr\", \"span\"::text AS \"span\", \
             \"price\"::text AS \"price\", \"n\", \"oid\" \
             FROM \"tracks\" LIMIT 100 OFFSET 200"
        );
        // Fetched whole: nothing about them is a preview.
        assert_eq!(plan.length_columns, 0);
        assert!(plan.columns.iter().all(|c| !c.previewed));
    }

    #[test]
    fn sqlserver_reads_only_hierarchyid_keys_as_text() {
        // `CAST(… AS nvarchar)` is lossy for what an alias type can stand
        // for, so SQL Server reads a key as text only for `hierarchyid`,
        // whose text is what it parses back.
        let columns = [
            col("node", "hierarchyid", Some(1)),
            col("code", "nvarchar(10)", None),
            col("stamp", "dbo.StampAlias", None),
            col("v", "sql_variant", None),
        ];
        let (sql, _params, plan) = base().select_bounded_sql(
            Dialect::SqlServer,
            &columns,
            &["node", "code", "stamp", "v"],
            512,
        );
        assert_eq!(
            sql,
            "SELECT CAST(\"node\" AS nvarchar(max)) AS \"node\", \"code\", \"stamp\", \"v\" \
             FROM \"tracks\" ORDER BY (SELECT NULL) OFFSET 200 ROWS FETCH NEXT 100 ROWS ONLY"
        );
        assert_eq!(plan.length_columns, 0);
    }

    #[test]
    fn sqlite_keys_are_never_read_as_text() {
        let columns = [col("k", "TEXT", Some(1)), col("n", "REAL", None)];
        let (sql, _params, _plan) =
            base().select_bounded_sql(Dialect::Sqlite, &columns, &["k"], 2048);
        assert_eq!(sql, "SELECT * FROM \"tracks\" LIMIT 100 OFFSET 200");
    }

    #[test]
    fn a_sort_on_a_key_read_as_text_names_the_tables_column() {
        // Both engines resolve a bare `ORDER BY "c"` to the output column
        // `… AS "c"`, which sorts a range or `hierarchyid` by its text. A
        // key read as text is sorted by the table's own column; a previewed
        // column and a plain one stay unqualified (a previewed `json` column
        // has no ordering of its own to name).
        let columns = [
            col("span", "int4range", Some(1)),
            col("body", "text", None),
            col("n", "integer", None),
        ];
        let sorted = |column: &str, dialect: Dialect| {
            let req = PageRequest {
                schema: Some("app".into()),
                sort: Some((column.into(), SortDir::Desc)),
                ..base()
            };
            req.select_bounded_sql(dialect, &columns, &["span"], 512).0
        };
        for (column, order) in [
            ("span", " ORDER BY \"app\".\"tracks\".\"span\" DESC "),
            ("body", " ORDER BY \"body\" DESC "),
            ("n", " ORDER BY \"n\" DESC "),
        ] {
            let sql = sorted(column, Dialect::Postgres);
            assert!(sql.contains(order), "{column}: {sql}");
        }
        let hierarchy = [col("node", "hierarchyid", Some(1))];
        let req = PageRequest {
            sort: Some(("node".into(), SortDir::Asc)),
            ..base()
        };
        let (sql, _, _) = req.select_bounded_sql(Dialect::SqlServer, &hierarchy, &["node"], 512);
        assert!(
            sql.contains(" ORDER BY \"tracks\".\"node\" ASC OFFSET"),
            "{sql}"
        );
    }

    #[test]
    fn bounded_select_with_no_columns_is_plain() {
        // No metadata yet (schema still loading): behave exactly as before.
        let (sql, _params, plan) = base().select_bounded_sql(Dialect::Sqlite, &[], &[], 2048);
        assert_eq!(sql, "SELECT * FROM \"tracks\" LIMIT 100 OFFSET 200");
        assert_eq!(plan.length_columns, 0);
        assert!(plan.columns.is_empty());
    }

    fn column(name: &str) -> ColumnInfo {
        ColumnInfo { name: name.into() }
    }

    #[test]
    fn assemble_strips_length_columns_and_flags_truncated_cells() {
        // Plan: id (scalar), body (text preview) → one length column.
        let plan = BoundedPlan {
            columns: vec![
                BoundedColumn {
                    name: "id".into(),
                    class: ColumnClass::Scalar,
                    previewed: false,
                },
                BoundedColumn {
                    name: "body".into(),
                    class: ColumnClass::Text,
                    previewed: true,
                },
            ],
            length_columns: 1,
        };
        let long: String = "x".repeat(3000);
        let raw = QueryResult {
            columns: vec![column("id"), column("body"), column("length(body)")],
            rows: vec![
                // Short value: complete, no preview.
                vec![
                    Value::Integer(1),
                    Value::Text("short".into()),
                    Value::Integer(5),
                ],
                // Long value: preview + truncation flag with the real length.
                vec![
                    Value::Integer(2),
                    Value::Text(long.clone()),
                    Value::Integer(3000),
                ],
                // NULL value: no preview at all.
                vec![Value::Integer(3), Value::Null, Value::Null],
            ],
        };
        let page = plan.assemble(raw, 2048);
        // Length column is gone.
        assert_eq!(page.result.columns.len(), 2);
        assert_eq!(page.result.rows[0].len(), 2);
        // Row 0: complete short text, no preview marker.
        assert_eq!(page.previews[0], vec![None, None]);
        // Row 1: previewed and trimmed to the cap; marker carries the full len.
        assert_eq!(
            page.previews[1][1],
            Some(PreviewInfo {
                full_len: 3000,
                binary: false
            })
        );
        if let Value::Text(t) = &page.result.rows[1][1] {
            assert_eq!(t.chars().count(), 2048, "preview trimmed to the cap");
        } else {
            panic!("expected text preview");
        }
        // Row 2: NULL is never a preview.
        assert_eq!(page.previews[2], vec![None, None]);
        assert!(page.has_truncation());
    }

    #[test]
    fn assemble_reports_binary_size_and_leaves_small_blobs_alone() {
        let plan = BoundedPlan {
            columns: vec![BoundedColumn {
                name: "blob".into(),
                class: ColumnClass::Binary,
                previewed: true,
            }],
            length_columns: 1,
        };
        let raw = QueryResult {
            columns: vec![column("blob"), column("length(blob)")],
            rows: vec![
                // A big blob: only a preview prefix fetched, full size known.
                vec![Value::Blob(vec![0u8; 2048]), Value::Integer(5_000_000)],
                // A small blob: complete, no marker.
                vec![Value::Blob(vec![1u8; 10]), Value::Integer(10)],
            ],
        };
        let page = plan.assemble(raw, 2048);
        assert_eq!(
            page.previews[0][0],
            Some(PreviewInfo {
                full_len: 5_000_000,
                binary: true
            })
        );
        assert_eq!(page.previews[1][0], None);
    }
}
