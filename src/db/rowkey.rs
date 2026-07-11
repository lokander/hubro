//! Row identity: how a single row of a table can be addressed safely.
//!
//! Editing must never target the wrong rows, so every UPDATE/DELETE needs a
//! set of columns that uniquely identifies one row. [`detect_row_identity`]
//! derives that from introspection metadata; [`update_sql`] / [`delete_sql`]
//! build parameterized statements targeting the *full* key. The final
//! safety net is [`DbPool::execute_checked`](super::DbPool::execute_checked),
//! which rolls back unless the affected-row count matches expectations.

use super::page::{quote_ident, Dialect};
use super::schema::{TableKind, TableMeta};

/// How rows of one table are uniquely addressed by UPDATE/DELETE, in
/// preference order: primary key, then a usable unique index, then (SQLite
/// only) the implicit rowid.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RowIdentity {
    /// The table's primary key; columns in key order.
    PrimaryKey { columns: Vec<String> },
    /// A unique index over NOT NULL columns, standing in for a missing PK.
    UniqueIndex { name: String, columns: Vec<String> },
    /// SQLite's implicit rowid, reached through `column` — normally
    /// `rowid`, or `_rowid_`/`oid` when user columns shadow the earlier
    /// names.
    ///
    /// Caveat: a rowid is not a stable handle. VACUUM can renumber rowids,
    /// and a concurrent writer can delete a row and have its rowid reused
    /// by a new INSERT. Editing (FRE-24) must fresh-read the row (rowid
    /// included) right before building the guarded write, not reuse a
    /// rowid captured earlier.
    Rowid { column: String },
}

impl RowIdentity {
    /// The WHERE-clause columns that pin down one row, in bind order.
    pub fn key_columns(&self) -> Vec<&str> {
        match self {
            RowIdentity::PrimaryKey { columns } | RowIdentity::UniqueIndex { columns, .. } => {
                columns.iter().map(String::as_str).collect()
            }
            RowIdentity::Rowid { column } => vec![column.as_str()],
        }
    }
}

/// Determines how rows of `table` can be uniquely addressed, or `None` when
/// they cannot (the table must then be treated as read-only).
///
/// - Views have no addressable rows: always `None`.
/// - A primary key wins. This covers `WITHOUT ROWID` tables too — they are
///   required to declare a PK, so they never reach the rowid fallback. Note
///   that on SQLite an `INTEGER PRIMARY KEY` column *is* the rowid, so using
///   the PK is exactly equivalent. (SQLite rowid tables historically allow
///   NULLs in non-INTEGER PKs; a NULL key value simply matches nothing with
///   `=`, so a guarded write aborts instead of hitting a wrong row.)
/// - Without a PK, the first non-partial unique index whose columns all
///   exist as real NOT NULL columns is used. Partial indexes
///   (`CREATE UNIQUE INDEX … WHERE …`) are disqualified outright: they only
///   guarantee uniqueness among rows matching their predicate, so duplicates
///   can exist across the rest of the table. Expression entries (surfaced
///   as `"<expr>"` by introspection) have no bindable column and disqualify
///   the index. Nullable columns disqualify it too: SQL NULLs compare
///   unequal, so a unique index still admits multiple rows that differ only
///   in NULLs — such rows cannot be uniquely addressed by `col = ?`.
/// - SQLite tables with neither are necessarily rowid tables, so the
///   implicit rowid addresses their rows — unless user columns shadow every
///   accessor name (`rowid`, `_rowid_`, `oid`), a pathological case that
///   yields `None` rather than risking a write against the wrong column.
/// - Postgres tables with neither yield `None`.
pub fn detect_row_identity(table: &TableMeta, dialect: Dialect) -> Option<RowIdentity> {
    if table.kind == TableKind::View {
        return None;
    }
    let pk: Vec<String> = table.primary_key().iter().map(|c| c.name.clone()).collect();
    if !pk.is_empty() {
        return Some(RowIdentity::PrimaryKey { columns: pk });
    }
    for index in table.indexes.iter().filter(|i| i.unique && !i.partial) {
        let usable = !index.columns.is_empty()
            && index
                .columns
                .iter()
                .all(|name| table.columns.iter().any(|c| c.name == *name && !c.nullable));
        if usable {
            return Some(RowIdentity::UniqueIndex {
                name: index.name.clone(),
                columns: index.columns.clone(),
            });
        }
    }
    match dialect {
        Dialect::Sqlite => rowid_accessor(table).map(|column| RowIdentity::Rowid { column }),
        Dialect::Postgres => None,
    }
}

/// First rowid accessor name not shadowed by a user column. SQLite resolves
/// `rowid`/`_rowid_`/`oid` to an explicit column of that name when one
/// exists (matching case-insensitively), so a shadowed name must not be
/// used to address the implicit rowid.
fn rowid_accessor(table: &TableMeta) -> Option<String> {
    ["rowid", "_rowid_", "oid"]
        .into_iter()
        .find(|alias| {
            !table
                .columns
                .iter()
                .any(|c| c.name.eq_ignore_ascii_case(alias))
        })
        .map(str::to_string)
}

/// Parameterized UPDATE targeting exactly one row through the full key.
///
/// Returns the SQL plus the parameter sources in bind order: one entry per
/// placeholder, naming the column whose value must be bound — the new
/// values for `set_columns` first, then the key values for the WHERE
/// clause (`rowid` for [`RowIdentity::Rowid`]).
///
/// `set_columns` must not be empty (there is nothing to update otherwise).
pub fn update_sql(
    table: &TableMeta,
    identity: &RowIdentity,
    set_columns: &[String],
    dialect: Dialect,
) -> (String, Vec<String>) {
    debug_assert!(!set_columns.is_empty(), "UPDATE needs at least one SET");
    let mut params = Vec::new();
    let assignments: Vec<String> = set_columns
        .iter()
        .map(|column| {
            format!(
                "{} = {}",
                quote_ident(column),
                placeholder(dialect, &mut params, column)
            )
        })
        .collect();
    let sql = format!(
        "UPDATE {} SET {} WHERE {}",
        qualified_table(table),
        assignments.join(", "),
        key_clause(identity, dialect, &mut params),
    );
    (sql, params)
}

/// Parameterized DELETE targeting exactly one row through the full key.
/// Parameter sources follow the same convention as [`update_sql`] (key
/// values only, in key order).
pub fn delete_sql(
    table: &TableMeta,
    identity: &RowIdentity,
    dialect: Dialect,
) -> (String, Vec<String>) {
    let mut params = Vec::new();
    let sql = format!(
        "DELETE FROM {} WHERE {}",
        qualified_table(table),
        key_clause(identity, dialect, &mut params),
    );
    (sql, params)
}

/// `"k1" = ? AND "k2" = ?` over the full key, appending parameter sources.
fn key_clause(identity: &RowIdentity, dialect: Dialect, params: &mut Vec<String>) -> String {
    identity
        .key_columns()
        .iter()
        .map(|column| {
            format!(
                "{} = {}",
                quote_ident(column),
                placeholder(dialect, params, column)
            )
        })
        .collect::<Vec<_>>()
        .join(" AND ")
}

/// Next placeholder (`?` / `$n`), recording which column it binds.
fn placeholder(dialect: Dialect, params: &mut Vec<String>, column: &str) -> String {
    params.push(column.to_string());
    match dialect {
        Dialect::Sqlite => "?".to_string(),
        Dialect::Postgres => format!("${}", params.len()),
    }
}

fn qualified_table(table: &TableMeta) -> String {
    match &table.schema {
        Some(schema) => format!("{}.{}", quote_ident(schema), quote_ident(&table.name)),
        None => quote_ident(&table.name),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::schema::{ColumnMeta, Generated, IndexMeta};

    fn col(name: &str, nullable: bool, pk: Option<u32>) -> ColumnMeta {
        ColumnMeta {
            name: name.into(),
            type_name: "TEXT".into(),
            nullable,
            primary_key_position: pk,
            default: None,
            generated: Generated::Never,
        }
    }

    fn index(name: &str, unique: bool, columns: &[&str]) -> IndexMeta {
        IndexMeta {
            name: name.into(),
            unique,
            partial: false,
            columns: columns.iter().map(|c| c.to_string()).collect(),
        }
    }

    fn partial_index(name: &str, unique: bool, columns: &[&str]) -> IndexMeta {
        IndexMeta {
            partial: true,
            ..index(name, unique, columns)
        }
    }

    fn table(kind: TableKind, columns: Vec<ColumnMeta>, indexes: Vec<IndexMeta>) -> TableMeta {
        TableMeta {
            schema: None,
            name: "t".into(),
            kind,
            columns,
            indexes,
            foreign_keys: vec![],
        }
    }

    #[test]
    fn single_column_pk_wins() {
        let t = table(
            TableKind::Table,
            vec![col("id", false, Some(1)), col("name", true, None)],
            vec![index("uniq_name", true, &["name"])],
        );
        for dialect in [Dialect::Sqlite, Dialect::Postgres] {
            assert_eq!(
                detect_row_identity(&t, dialect),
                Some(RowIdentity::PrimaryKey {
                    columns: vec!["id".into()]
                })
            );
        }
    }

    #[test]
    fn composite_pk_is_ordered_by_key_position() {
        let t = table(
            TableKind::Table,
            vec![
                col("b", false, Some(2)),
                col("x", true, None),
                col("a", false, Some(1)),
            ],
            vec![],
        );
        assert_eq!(
            detect_row_identity(&t, Dialect::Postgres),
            Some(RowIdentity::PrimaryKey {
                columns: vec!["a".into(), "b".into()]
            })
        );
    }

    #[test]
    fn without_rowid_style_table_uses_its_pk_not_the_fallback() {
        // A WITHOUT ROWID table is required to declare a PK, so metadata-wise
        // it is indistinguishable from any other PK table — it must resolve
        // to the PK, never to the (nonexistent) rowid.
        let t = table(
            TableKind::Table,
            vec![
                col("key", false, Some(1)),
                col("scope", false, Some(2)),
                col("value", true, None),
            ],
            vec![],
        );
        assert_eq!(
            detect_row_identity(&t, Dialect::Sqlite),
            Some(RowIdentity::PrimaryKey {
                columns: vec!["key".into(), "scope".into()]
            })
        );
    }

    #[test]
    fn not_null_unique_index_is_the_pk_fallback() {
        let t = table(
            TableKind::Table,
            vec![col("email", false, None), col("bio", true, None)],
            vec![
                index("not_unique", false, &["bio"]),
                index("uniq_email", true, &["email"]),
            ],
        );
        assert_eq!(
            detect_row_identity(&t, Dialect::Postgres),
            Some(RowIdentity::UniqueIndex {
                name: "uniq_email".into(),
                columns: vec!["email".into()]
            })
        );
    }

    #[test]
    fn nullable_unique_index_is_rejected() {
        // NULLs compare unequal, so the index admits rows that `col = ?`
        // can never single out.
        let t = table(
            TableKind::Table,
            vec![col("email", true, None)],
            vec![index("uniq_email", true, &["email"])],
        );
        assert_eq!(
            detect_row_identity(&t, Dialect::Postgres),
            None,
            "postgres: nullable unique index must not make the table editable"
        );
        assert_eq!(
            detect_row_identity(&t, Dialect::Sqlite),
            Some(RowIdentity::Rowid {
                column: "rowid".into()
            }),
            "sqlite: falls through to the rowid instead"
        );
    }

    #[test]
    fn partial_unique_index_is_rejected() {
        // `CREATE UNIQUE INDEX … WHERE …` only enforces uniqueness inside
        // its predicate; duplicates can exist across the rest of the table,
        // so it must never serve as row identity.
        let t = table(
            TableKind::Table,
            vec![col("email", false, None)],
            vec![partial_index("uniq_active_email", true, &["email"])],
        );
        assert_eq!(
            detect_row_identity(&t, Dialect::Postgres),
            None,
            "postgres: a partial unique index leaves the table read-only"
        );
        assert_eq!(
            detect_row_identity(&t, Dialect::Sqlite),
            Some(RowIdentity::Rowid {
                column: "rowid".into()
            }),
            "sqlite: falls through to the rowid instead"
        );
        // A later full unique index is still picked up.
        let recovered = table(
            TableKind::Table,
            vec![col("email", false, None)],
            vec![
                partial_index("uniq_active_email", true, &["email"]),
                index("uniq_email", true, &["email"]),
            ],
        );
        assert_eq!(
            detect_row_identity(&recovered, Dialect::Postgres),
            Some(RowIdentity::UniqueIndex {
                name: "uniq_email".into(),
                columns: vec!["email".into()]
            })
        );
    }

    #[test]
    fn expression_unique_index_is_rejected() {
        let t = table(
            TableKind::Table,
            vec![col("email", false, None)],
            vec![index("uniq_lower_email", true, &["<expr>"])],
        );
        assert_eq!(detect_row_identity(&t, Dialect::Postgres), None);
    }

    #[test]
    fn partially_usable_unique_index_is_rejected_whole() {
        // One NOT NULL column does not save an index whose other column is
        // nullable — the *full* key must be usable.
        let t = table(
            TableKind::Table,
            vec![col("a", false, None), col("b", true, None)],
            vec![index("uniq_ab", true, &["a", "b"])],
        );
        assert_eq!(detect_row_identity(&t, Dialect::Postgres), None);
    }

    #[test]
    fn later_usable_unique_index_is_found_past_unusable_ones() {
        let t = table(
            TableKind::Table,
            vec![col("a", true, None), col("b", false, None)],
            vec![
                index("uniq_nullable", true, &["a"]),
                index("uniq_expr", true, &["<expr>"]),
                index("uniq_good", true, &["b"]),
            ],
        );
        assert_eq!(
            detect_row_identity(&t, Dialect::Postgres),
            Some(RowIdentity::UniqueIndex {
                name: "uniq_good".into(),
                columns: vec!["b".into()]
            })
        );
    }

    #[test]
    fn keyless_sqlite_table_falls_back_to_rowid() {
        let t = table(TableKind::Table, vec![col("data", true, None)], vec![]);
        assert_eq!(
            detect_row_identity(&t, Dialect::Sqlite),
            Some(RowIdentity::Rowid {
                column: "rowid".into()
            })
        );
    }

    #[test]
    fn keyless_postgres_table_is_read_only() {
        let t = table(TableKind::Table, vec![col("data", true, None)], vec![]);
        assert_eq!(detect_row_identity(&t, Dialect::Postgres), None);
    }

    #[test]
    fn views_are_never_addressable() {
        // Even when introspection reports PK-ish columns (it doesn't today),
        // a view must stay read-only.
        let t = table(TableKind::View, vec![col("id", false, Some(1))], vec![]);
        assert_eq!(detect_row_identity(&t, Dialect::Sqlite), None);
        assert_eq!(detect_row_identity(&t, Dialect::Postgres), None);
    }

    #[test]
    fn shadowed_rowid_names_pick_the_next_accessor() {
        let t = table(
            TableKind::Table,
            vec![col("ROWID", true, None)], // shadows case-insensitively
            vec![],
        );
        assert_eq!(
            detect_row_identity(&t, Dialect::Sqlite),
            Some(RowIdentity::Rowid {
                column: "_rowid_".into()
            })
        );
        let all_shadowed = table(
            TableKind::Table,
            vec![
                col("rowid", true, None),
                col("_rowid_", true, None),
                col("oid", true, None),
            ],
            vec![],
        );
        assert_eq!(
            detect_row_identity(&all_shadowed, Dialect::Sqlite),
            None,
            "no safe accessor left — read-only beats writing the wrong column"
        );
    }

    #[test]
    fn update_sql_targets_the_full_composite_key_sqlite() {
        let t = table(
            TableKind::Table,
            vec![
                col("artist_id", false, Some(1)),
                col("seq", false, Some(2)),
                col("title", false, None),
            ],
            vec![],
        );
        let identity = detect_row_identity(&t, Dialect::Sqlite).unwrap();
        let (sql, params) = update_sql(&t, &identity, &["title".into()], Dialect::Sqlite);
        assert_eq!(
            sql,
            "UPDATE \"t\" SET \"title\" = ? WHERE \"artist_id\" = ? AND \"seq\" = ?"
        );
        assert_eq!(params, ["title", "artist_id", "seq"]);
    }

    #[test]
    fn update_sql_uses_dollar_placeholders_and_schema_on_postgres() {
        let mut t = table(
            TableKind::Table,
            vec![col("id", false, Some(1)), col("na\"me", true, None)],
            vec![],
        );
        t.schema = Some("app data".into());
        let identity = detect_row_identity(&t, Dialect::Postgres).unwrap();
        let (sql, params) = update_sql(&t, &identity, &["na\"me".into()], Dialect::Postgres);
        assert_eq!(
            sql,
            "UPDATE \"app data\".\"t\" SET \"na\"\"me\" = $1 WHERE \"id\" = $2"
        );
        assert_eq!(params, ["na\"me", "id"]);
    }

    #[test]
    fn update_and_delete_sql_use_the_rowid_form() {
        let t = table(TableKind::Table, vec![col("data", true, None)], vec![]);
        let identity = detect_row_identity(&t, Dialect::Sqlite).unwrap();
        let (sql, params) = update_sql(&t, &identity, &["data".into()], Dialect::Sqlite);
        assert_eq!(sql, "UPDATE \"t\" SET \"data\" = ? WHERE \"rowid\" = ?");
        assert_eq!(params, ["data", "rowid"]);
        let (sql, params) = delete_sql(&t, &identity, Dialect::Sqlite);
        assert_eq!(sql, "DELETE FROM \"t\" WHERE \"rowid\" = ?");
        assert_eq!(params, ["rowid"]);
    }

    #[test]
    fn delete_sql_targets_the_full_key_on_postgres() {
        let mut t = table(
            TableKind::Table,
            vec![col("region", false, Some(1)), col("slot", false, Some(2))],
            vec![],
        );
        t.schema = Some("warehouse".into());
        t.name = "locations".into();
        let identity = detect_row_identity(&t, Dialect::Postgres).unwrap();
        let (sql, params) = delete_sql(&t, &identity, Dialect::Postgres);
        assert_eq!(
            sql,
            "DELETE FROM \"warehouse\".\"locations\" WHERE \"region\" = $1 AND \"slot\" = $2"
        );
        assert_eq!(params, ["region", "slot"]);
    }

    #[test]
    fn unique_index_identity_targets_its_columns() {
        let t = table(
            TableKind::Table,
            vec![col("email", false, None), col("bio", true, None)],
            vec![index("uniq_email", true, &["email"])],
        );
        let identity = detect_row_identity(&t, Dialect::Postgres).unwrap();
        let (sql, params) = update_sql(&t, &identity, &["bio".into()], Dialect::Postgres);
        assert_eq!(sql, "UPDATE \"t\" SET \"bio\" = $1 WHERE \"email\" = $2");
        assert_eq!(params, ["bio", "email"]);
    }
}
