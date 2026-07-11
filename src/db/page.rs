//! SQL generation for paged table reads with optional sort and filter.
//! Identifier names come from introspection (still quoted defensively);
//! filter values are always bound parameters, never interpolated.

use super::value::Value;

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
}

impl Dialect {
    /// First bound-parameter placeholder (at most one is ever used here).
    fn placeholder(self) -> &'static str {
        match self {
            Dialect::Sqlite => "?",
            Dialect::Postgres => "$1",
        }
    }
}

impl PageRequest {
    /// SELECT for this page: SQL plus bound parameters.
    pub fn select_sql(&self, dialect: Dialect) -> (String, Vec<Value>) {
        let (where_clause, params) = self.where_clause(dialect);
        let order = match &self.sort {
            Some((column, dir)) => {
                format!(" ORDER BY {} {}", quote_ident(column), dir.sql())
            }
            None => String::new(),
        };
        let extra = match &self.extra_key_column {
            Some(column) => format!("{}, ", quote_ident(column)),
            None => String::new(),
        };
        let sql = format!(
            "SELECT {extra}* FROM {}{where_clause}{order} LIMIT {} OFFSET {}",
            self.qualified_table(),
            self.limit,
            self.offset,
        );
        (sql, params)
    }

    /// SELECT for exporting the current view: the same table, filter, and
    /// sort as [`Self::select_sql`], but WITHOUT `LIMIT`/`OFFSET` (every
    /// matching row) and without the hidden `extra_key_column` (exports show
    /// the visible `*` columns only). Row order still follows the active
    /// sort so a sorted export matches what the grid shows.
    pub fn export_sql(&self, dialect: Dialect) -> (String, Vec<Value>) {
        let (where_clause, params) = self.where_clause(dialect);
        let order = match &self.sort {
            Some((column, dir)) => {
                format!(" ORDER BY {} {}", quote_ident(column), dir.sql())
            }
            None => String::new(),
        };
        let sql = format!(
            "SELECT * FROM {}{where_clause}{order}",
            self.qualified_table(),
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
                // handles this implicitly). Fine for a viewer; indexes aren't a
                // concern yet.
                let quoted = match dialect {
                    Dialect::Sqlite => quote_ident(column),
                    Dialect::Postgres => format!("{}::text", quote_ident(column)),
                };
                let placeholder = dialect.placeholder();
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
            Some(Filter::Equalities(pairs)) => {
                if pairs.is_empty() {
                    return (String::new(), Vec::new());
                }
                // Same text-comparison strategy as the equals filter: cast the
                // column to text on Postgres (so exotic types the viewer only
                // ever sees as text — uuid, enums, timestamps — still compare)
                // and bind each value's text form. SQLite affinity coerces.
                let mut clauses = Vec::with_capacity(pairs.len());
                let mut params = Vec::with_capacity(pairs.len());
                for (column, value) in pairs {
                    let quoted = match dialect {
                        Dialect::Sqlite => quote_ident(column),
                        Dialect::Postgres => format!("{}::text", quote_ident(column)),
                    };
                    let placeholder = match dialect {
                        Dialect::Sqlite => "?".to_string(),
                        Dialect::Postgres => format!("${}", params.len() + 1),
                    };
                    clauses.push(format!("{quoted} = {placeholder}"));
                    params.push(Value::Text(equality_text(value)));
                }
                (format!(" WHERE {}", clauses.join(" AND ")), params)
            }
        }
    }
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
}
