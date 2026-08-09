//! Row identity: how a single row of a table can be addressed safely.
//!
//! Editing must never target the wrong rows, so every UPDATE/DELETE needs a
//! set of columns that uniquely identifies one row. [`detect_row_identity`]
//! derives that from introspection metadata; the value-aware statement
//! builders in [`super::staged`] then target the *full* key it reports. The
//! final safety net is
//! [`DbPool::execute_checked`](super::DbPool::execute_checked), which rolls
//! back unless the affected-row count matches expectations.

use super::schema::{TableKind, TableMeta};
use super::sql::Dialect;

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
/// - Postgres and SQL Server tables with neither yield `None` (neither has
///   a rowid analogue reachable through plain SQL).
pub fn detect_row_identity(table: &TableMeta, dialect: Dialect) -> Option<RowIdentity> {
    // Views and materialized views are read-only; even if a matview carries a
    // unique index, we must not offer editing that UPDATE/DELETE can't perform.
    if matches!(table.kind, TableKind::View | TableKind::MaterializedView) {
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
        Dialect::Postgres | Dialect::SqlServer => None,
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
            type_detail: crate::db::TypeDetail::Plain,
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
            restriction: None,
            internal: None,
            kind_label: None,
        }
    }

    #[test]
    fn single_column_pk_wins() {
        let t = table(
            TableKind::Table,
            vec![col("id", false, Some(1)), col("name", true, None)],
            vec![index("uniq_name", true, &["name"])],
        );
        for dialect in [Dialect::Sqlite, Dialect::Postgres, Dialect::SqlServer] {
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
    fn keyless_sqlserver_table_is_read_only() {
        // No rowid analogue: PK (or usable unique index) or refuse, like
        // Postgres.
        let t = table(TableKind::Table, vec![col("data", true, None)], vec![]);
        assert_eq!(detect_row_identity(&t, Dialect::SqlServer), None);
        // A usable unique index still qualifies.
        let indexed = table(
            TableKind::Table,
            vec![col("email", false, None)],
            vec![index("uniq_email", true, &["email"])],
        );
        assert_eq!(
            detect_row_identity(&indexed, Dialect::SqlServer),
            Some(RowIdentity::UniqueIndex {
                name: "uniq_email".into(),
                columns: vec!["email".into()]
            })
        );
    }

    #[test]
    fn views_are_never_addressable() {
        // Even when introspection reports PK-ish columns (it doesn't today),
        // a view must stay read-only.
        let t = table(TableKind::View, vec![col("id", false, Some(1))], vec![]);
        for dialect in [Dialect::Sqlite, Dialect::Postgres, Dialect::SqlServer] {
            assert_eq!(detect_row_identity(&t, dialect), None);
        }
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
}
