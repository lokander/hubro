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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Filter {
    pub column: String,
    pub op: FilterOp,
    pub value: String,
}

/// One page of one table, with optional sort and filter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PageRequest {
    pub table: String,
    pub limit: u32,
    pub offset: u64,
    pub sort: Option<(String, SortDir)>,
    pub filter: Option<Filter>,
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
        let sql = format!(
            "SELECT * FROM {}{where_clause}{order} LIMIT {} OFFSET {}",
            quote_ident(&self.table),
            self.limit,
            self.offset,
        );
        (sql, params)
    }

    /// COUNT(*) with the same filter, for the row-count indicator.
    pub fn count_sql(&self, dialect: Dialect) -> (String, Vec<Value>) {
        let (where_clause, params) = self.where_clause(dialect);
        let sql = format!(
            "SELECT COUNT(*) FROM {}{where_clause}",
            quote_ident(&self.table)
        );
        (sql, params)
    }

    fn where_clause(&self, dialect: Dialect) -> (String, Vec<Value>) {
        let Some(filter) = &self.filter else {
            return (String::new(), Vec::new());
        };
        // Postgres compares strictly by type, so the column is cast to text
        // to match the text filter value (SQLite's affinity handles this
        // implicitly). Fine for a viewer; indexes aren't a concern yet.
        let column = match dialect {
            Dialect::Sqlite => quote_ident(&filter.column),
            Dialect::Postgres => format!("{}::text", quote_ident(&filter.column)),
        };
        let placeholder = dialect.placeholder();
        match filter.op {
            FilterOp::Equals => (
                format!(" WHERE {column} = {placeholder}"),
                vec![Value::Text(filter.value.clone())],
            ),
            FilterOp::Contains => (
                format!(" WHERE {column} LIKE {placeholder} ESCAPE '\\'"),
                vec![Value::Text(format!("%{}%", escape_like(&filter.value)))],
            ),
        }
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
            table: "tracks".into(),
            limit: 100,
            offset: 200,
            sort: None,
            filter: None,
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
        req.filter = Some(Filter {
            column: "name".into(),
            op: FilterOp::Equals,
            value: "Track 7".into(),
        });
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
        req.filter = Some(Filter {
            column: "name".into(),
            op: FilterOp::Contains,
            value: "50%_\\".into(),
        });
        let (sql, params) = req.select_sql(Dialect::Sqlite);
        assert_eq!(
            sql,
            "SELECT * FROM \"tracks\" WHERE \"name\" LIKE ? ESCAPE '\\' LIMIT 100 OFFSET 200"
        );
        assert_eq!(params, vec![Value::Text("%50\\%\\_\\\\%".into())]);
    }

    #[test]
    fn postgres_dialect_uses_dollar_placeholder_and_text_cast() {
        let mut req = base();
        req.filter = Some(Filter {
            column: "name".into(),
            op: FilterOp::Equals,
            value: "7".into(),
        });
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
        req.filter = Some(Filter {
            column: "name".into(),
            op: FilterOp::Contains,
            value: "abc".into(),
        });
        let (sql, params) = req.count_sql(Dialect::Sqlite);
        assert_eq!(
            sql,
            "SELECT COUNT(*) FROM \"tracks\" WHERE \"name\" LIKE ? ESCAPE '\\'"
        );
        assert_eq!(params.len(), 1);
    }
}
