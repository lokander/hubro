//! SQL generation for paged table reads with optional sort and filter.
//! Identifier names come from introspection (still quoted defensively);
//! filter values are always bound parameters, never interpolated.
//!
//! Two page shapes exist:
//!
//! - [`PageRequest::select_sql`] fetches every column in full (`SELECT *`).
//! - [`PageRequest::select_bounded_sql`] fetches a bounded *preview* of large
//!   columns (long text / json / blobs) instead of their full contents, so a
//!   page of multi-MB values never buffers megabytes per row (FRE-33). Small
//!   columns and identity/foreign-key columns are always fetched whole so row
//!   addressing and navigation stay exact.

use super::schema::ColumnMeta;
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

/// Classifies a declared column type into a [`ColumnClass`] by
/// case-insensitive substring, mirroring the editor-kind rules so both
/// backends map without a per-type table. Anything not clearly text or
/// binary — and every recognized scalar — stays [`ColumnClass::Scalar`] so it
/// is fetched whole (previewing a scalar would corrupt its decoded type and,
/// worse, any locator built from it).
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
    const SCALAR_HINTS: [&str; 16] = [
        "int", "serial", "real", "float", "double", "numeric", "decimal", "money", "bool", "date",
        "time", "uuid", "bit", "oid", "point", "interval",
    ];
    if SCALAR_HINTS.iter().any(|hint| t.contains(hint)) && !t.contains("json") {
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
    // Unknown/user-defined types could hold arbitrarily large text, so
    // preview them rather than buffer them in full.
    ColumnClass::Text
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
///   installed by foreign-key navigation (FRE-29) to pin the referenced row
///   across one or more columns. Values come from the source row, so they are
///   already typed [`Value`]s rather than user text.
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
    Equalities(Vec<(String, Value)>),
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

/// SQL flavor differences the page builder must care about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dialect {
    Sqlite,
    Postgres,
    SqlServer,
}

impl Dialect {
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
    /// Postgres uses the postfix `expr::type` shape; SQLite shares the arm
    /// but no builder ever casts there (its type affinity coerces on its
    /// own). SQL Server uses the prefix `CAST(expr AS type)` shape.
    ///
    /// Callers pass a **dialect-neutral** target for the generic cases and
    /// this method translates it: the stringify target `"text"` becomes
    /// `nvarchar(max)` on SQL Server (T-SQL has no `text`-as-cast-target
    /// worth using — the legacy `text` type is deprecated and not
    /// comparable). Any other target renders verbatim.
    pub(crate) fn cast_expr(self, expr: &str, target: &str) -> String {
        match self {
            Dialect::Sqlite | Dialect::Postgres => format!("{expr}::{target}"),
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
    /// memory does not scale with value size (FRE-33). Returns the SQL, its
    /// bound parameters, and the [`BoundedPlan`] the decoder needs to strip
    /// the trailing `length` helper columns and rebuild the visible result.
    ///
    /// `columns` are the table's visible columns in `*` order (from
    /// introspection); `no_preview` names columns that must always be fetched
    /// whole (identity keys and foreign-key columns — a truncated key would
    /// misaddress rows or misdirect a foreign-key jump). Falls back to the
    /// plain [`Self::select_sql`] (empty plan) when nothing qualifies for a
    /// preview, so callers without metadata behave exactly as before.
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
        // Nothing large to bound → identical to the plain page select.
        if columns.is_empty() || !specs.iter().any(|s| s.previewed) {
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
                value_exprs.push(q);
                continue;
            }
            match (dialect, spec.class) {
                (Dialect::Sqlite, ColumnClass::Text | ColumnClass::Binary) => {
                    value_exprs.push(format!("substr({q}, 1, {n}) AS {q}"));
                    // SQLite length() is characters for text, bytes for blobs.
                    length_exprs.push(format!("length({q})"));
                }
                (Dialect::Postgres, ColumnClass::Text) => {
                    let cast = dialect.cast_expr(&q, "text");
                    value_exprs.push(format!("left({cast}, {n}) AS {q}"));
                    length_exprs.push(format!("length({cast})"));
                }
                (Dialect::Postgres, ColumnClass::Binary) => {
                    value_exprs.push(format!("substring({q} from 1 for {n}) AS {q}"));
                    length_exprs.push(format!("octet_length({q})"));
                }
                (Dialect::SqlServer, ColumnClass::Text) => {
                    // Cast to nvarchar(max) so xml/legacy types preview too;
                    // LEN counts characters (like Postgres length()).
                    let cast = dialect.cast_expr(&q, "text");
                    value_exprs.push(format!("SUBSTRING({cast}, 1, {n}) AS {q}"));
                    length_exprs.push(format!("LEN({cast})"));
                }
                (Dialect::SqlServer, ColumnClass::Binary) => {
                    // SUBSTRING works on varbinary; DATALENGTH is bytes.
                    value_exprs.push(format!("SUBSTRING({q}, 1, {n}) AS {q}"));
                    length_exprs.push(format!("DATALENGTH({q})"));
                }
                (_, ColumnClass::Scalar) => value_exprs.push(q),
            }
        }
        let length_columns = length_exprs.len();
        let mut select_list = value_exprs.join(", ");
        if !length_exprs.is_empty() {
            select_list.push_str(", ");
            select_list.push_str(&length_exprs.join(", "));
        }
        let (where_clause, params) = self.where_clause(dialect);
        let sql = format!(
            "SELECT {select_list} FROM {}{where_clause}{}{}",
            self.qualified_table(),
            self.order_clause(),
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

    fn qualified_table(&self) -> String {
        match &self.schema {
            Some(schema) => format!("{}.{}", quote_ident(schema), quote_ident(&self.table)),
            None => quote_ident(&self.table),
        }
    }

    fn where_clause(&self, dialect: Dialect) -> (String, Vec<Value>) {
        match &self.filter {
            None => (String::new(), Vec::new()),
            Some(Filter::Column { column, op, value }) => {
                // Postgres compares strictly by type, so the column is cast to
                // text to match the text filter value (SQLite's affinity
                // handles this implicitly; SQL Server casts to nvarchar(max)
                // the same way). Fine for a viewer; indexes aren't a concern
                // yet.
                let quoted = match dialect {
                    Dialect::Sqlite => quote_ident(column),
                    Dialect::Postgres | Dialect::SqlServer => {
                        dialect.cast_expr(&quote_ident(column), "text")
                    }
                };
                let placeholder = dialect.placeholder(1);
                match op {
                    FilterOp::Equals => (
                        format!(" WHERE {quoted} = {placeholder}"),
                        vec![Value::Text(value.clone())],
                    ),
                    FilterOp::Contains => (
                        format!(" WHERE {quoted} LIKE {placeholder} ESCAPE '\\'"),
                        vec![Value::Text(format!("%{}%", escape_like(value)))],
                    ),
                }
            }
            Some(Filter::Equalities(pairs)) => equalities_where(pairs, dialect),
        }
    }
}

/// A ` WHERE k1 = ? AND k2 = ?` clause pinning one row by its identity
/// columns, with each value bound as text (Postgres/SQL Server cast the
/// column to text; SQLite affinity coerces) — the same strategy the equals filter and
/// foreign-key navigation use, so exotic key types (uuid, enums, timestamps)
/// still compare. An empty list matches every row (no clause). Shared by the
/// [`Filter::Equalities`] page filter and [`super::DbPool::fetch_cell`].
pub(crate) fn equalities_where(
    pairs: &[(String, Value)],
    dialect: Dialect,
) -> (String, Vec<Value>) {
    if pairs.is_empty() {
        return (String::new(), Vec::new());
    }
    let mut clauses = Vec::with_capacity(pairs.len());
    let mut params = Vec::with_capacity(pairs.len());
    for (column, value) in pairs {
        let quoted = match dialect {
            Dialect::Sqlite => quote_ident(column),
            Dialect::Postgres | Dialect::SqlServer => {
                dialect.cast_expr(&quote_ident(column), "text")
            }
        };
        let placeholder = dialect.placeholder(params.len() + 1);
        clauses.push(format!("{quoted} = {placeholder}"));
        params.push(Value::Text(equality_text(value)));
    }
    (format!(" WHERE {}", clauses.join(" AND ")), params)
}

/// The text form bound for an equality comparison. Mirrors how the equals
/// filter binds text and lets SQLite affinity / the Postgres `::text` cast do
/// the matching. FK values are realistically integers or text; NULL is
/// guarded out before a jump is built, and blobs never key a foreign key.
fn equality_text(value: &Value) -> String {
    match value {
        Value::Integer(i) => i.to_string(),
        Value::Real(r) => r.to_string(),
        Value::Text(t) => t.clone(),
        Value::Null => String::new(),
        Value::Blob(_) => value.display(),
    }
}

/// Escapes LIKE wildcards in user input so "50%" matches literally.
fn escape_like(needle: &str) -> String {
    needle
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}

/// Double-quotes an identifier, doubling embedded quotes. Deliberately takes
/// no [`Dialect`]: ANSI `"…"` quoting works on SQLite, Postgres, and SQL
/// Server (with QUOTED_IDENTIFIER ON, which tiberius defaults to), so one
/// dialect-independent form covers every backend we would add.
pub(crate) fn quote_ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
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
    fn sqlserver_equalities_filter_casts_and_numbers_placeholders() {
        let mut req = base();
        req.filter = Some(Filter::Equalities(vec![
            ("region".into(), Value::Text("eu".into())),
            ("slot".into(), Value::Integer(3)),
        ]));
        let (sql, params) = req.select_sql(Dialect::SqlServer);
        assert_eq!(
            sql,
            "SELECT * FROM \"tracks\" \
             WHERE CAST(\"region\" AS nvarchar(max)) = @P1 \
             AND CAST(\"slot\" AS nvarchar(max)) = @P2 \
             ORDER BY (SELECT NULL) OFFSET 200 ROWS FETCH NEXT 100 ROWS ONLY"
        );
        assert_eq!(
            params,
            vec![Value::Text("eu".into()), Value::Text("3".into())]
        );
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
            ("artist_id".into(), Value::Integer(1)),
            ("seq".into(), Value::Integer(2)),
        ]));
        let (sql, params) = req.select_sql(Dialect::Sqlite);
        assert_eq!(
            sql,
            "SELECT * FROM \"tracks\" WHERE \"artist_id\" = ? AND \"seq\" = ? \
             LIMIT 100 OFFSET 200"
        );
        // Values bind as text and rely on SQLite affinity, like the equals
        // filter.
        assert_eq!(
            params,
            vec![Value::Text("1".into()), Value::Text("2".into())]
        );
    }

    #[test]
    fn equalities_filter_casts_and_numbers_placeholders_on_postgres() {
        let mut req = base();
        req.filter = Some(Filter::Equalities(vec![
            ("region".into(), Value::Text("eu".into())),
            ("slot".into(), Value::Integer(3)),
        ]));
        let (sql, params) = req.select_sql(Dialect::Postgres);
        assert_eq!(
            sql,
            "SELECT * FROM \"tracks\" WHERE \"region\"::text = $1 AND \"slot\"::text = $2 \
             LIMIT 100 OFFSET 200"
        );
        assert_eq!(
            params,
            vec![Value::Text("eu".into()), Value::Text("3".into())]
        );
        // COUNT shares the same clause and binds.
        let (count_sql, count_params) = req.count_sql(Dialect::Postgres);
        assert_eq!(
            count_sql,
            "SELECT COUNT(*) FROM \"tracks\" WHERE \"region\"::text = $1 AND \"slot\"::text = $2"
        );
        assert_eq!(count_params.len(), 2);
    }

    #[test]
    fn equalities_filter_quotes_weird_identifiers() {
        let mut req = base();
        req.filter = Some(Filter::Equalities(vec![(
            "col\"name".into(),
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

    // ---- Bounded previews (FRE-33) ------------------------------------

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
        // tiberius codec (FRE-55/FRE-56), while the Text class routes them
        // through the `CAST(… AS nvarchar(max))` preview, which SQL Server
        // accepts for all of them (CLR UDTs stringify via their ToString).
        for t in ["sql_variant", "hierarchyid", "geography", "geometry"] {
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
             LEN(CAST(\"body\" AS nvarchar(max))), DATALENGTH(\"blob\") \
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
