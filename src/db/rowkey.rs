//! Row identity: how a single row of a table can be addressed safely.
//!
//! Editing must never target the wrong rows, so every UPDATE/DELETE needs a
//! set of columns that uniquely identifies one row. [`detect_row_identity`]
//! derives that from introspection metadata; the value-aware statement
//! builders in [`super::staged`] then target the *full* key it reports. The
//! final safety net is
//! [`DbPool::execute_checked`](super::DbPool::execute_checked), which rolls
//! back unless the affected-row count matches expectations.

use super::schema::{ColumnMeta, TableKind, TableMeta, TypeDetail};
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
    /// by a new INSERT. Editing must fresh-read the row (rowid
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
///
/// `rules` is the dialect, plus — on Postgres — which session settings the
/// connection pins ([`KeyRules`]); a bare [`Dialect`] means every pin holds.
pub fn detect_row_identity(table: &TableMeta, rules: impl Into<KeyRules>) -> Option<RowIdentity> {
    let rules = rules.into();
    let dialect = rules.dialect;
    // Views and materialized views are read-only; even if a matview carries a
    // unique index, we must not offer editing that UPDATE/DELETE can't perform.
    if matches!(table.kind, TableKind::View | TableKind::MaterializedView) {
        return None;
    }
    let pk = table.primary_key();
    if !pk.is_empty() && pk.iter().all(|c| key_column_addressable(c, rules)) {
        let columns = pk.iter().map(|c| c.name.clone()).collect();
        return Some(RowIdentity::PrimaryKey { columns });
    }
    for index in table.indexes.iter().filter(|i| i.unique && !i.partial) {
        let usable = !index.columns.is_empty()
            && index.columns.iter().all(|name| {
                table
                    .columns
                    .iter()
                    .any(|c| c.name == *name && !c.nullable && key_column_addressable(c, rules))
            });
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

/// Whether a value of `column`, as the grid reads it, can be bound back to
/// find its row — the condition for the column to serve in a row's identity
/// ([`detect_row_identity`]) or as the target of a foreign-key jump
/// ([`fk_target_columns`](super::fk::fk_target_columns)). Over plain data —
/// the column's type and the dialect — so a test can execute every answer.
///
/// **SQL Server: false for `sql_variant`.** Each value carries its own base
/// type, and a variant compares unequal to any value of another type family:
/// measured on SQL Server 2022, a key holding `CAST(5 AS int)` or a `date`
/// reads as text (or the `<ssvariant>` marker) and matches no row when bound
/// back, so its long cell loaded as NULL, its save was refused and its jump
/// showed an empty page — silently, since nothing failed. Only a variant that
/// happens to hold a string re-addresses, and whether one does is a property
/// of the row, not the column. Refusing the key leaves the table browsable and
/// read-only with the key-less table's reason, which the UI already handles.
/// The values decide, not the name: an alias type declared `FROM
/// sql_variant` is reported by its alias name, so its system type
/// ([`TypeDetail::Alias`]) is read too — measured, a key of such an alias
/// type missed its rows exactly as a plain `sql_variant` key did.
///
/// **Postgres: false for the `reg*` types whose text depends on
/// `search_path`** (`regclass`, `regtype`, `regproc`, …, and arrays of them).
/// A key without a decoder is read as `::text` by one pooled session and
/// parsed back by another ([`fetched_as_server_text`](super::page::fetched_as_server_text)),
/// and `search_path` — which a role or database default can change for the
/// sessions opened after it — decides both the spelling and what the spelling
/// resolves to. Measured on PostgreSQL 17, with a `t` in schemas `sp1` and
/// `sp2`: the key `sp2.t` prints as `t` under `search_path = sp2`, and `t`
/// under `search_path = sp1` is `sp1.t` — another row, which a one-row guard
/// cannot tell from the right one. The other settings the text depends on are
/// pinned for every pooled session instead ([`super::postgres::key_text_pins`]);
/// `search_path` is the user's name resolution, not hubro's to pin. A
/// `regnamespace` or `regrole` prints the bare name whatever the path, and
/// stays a key.
///
/// **SQLite: always true.** It echoes a declared type verbatim, so a
/// `sql_variant` or `regclass` there is just a name, and its values bind as
/// they read.
///
/// **Postgres, on a connection that could not pin a setting**: false for the
/// types whose text depends on it ([`text_depends_on`]). A pooler can refuse
/// the startup option that pins it — measured, PgBouncer 1.18 refuses
/// `options` outright, and 1.21 and 1.26 take `IntervalStyle` but refuse
/// `lc_monetary` — and then sessions opened before and after an `ALTER
/// ROLE … SET` print those types differently.
pub fn key_column_addressable(column: &ColumnMeta, rules: impl Into<KeyRules>) -> bool {
    let KeyRules { dialect, pinned } = rules.into();
    match dialect {
        Dialect::SqlServer => {
            let is_variant = |name: &str| name.trim().eq_ignore_ascii_case("sql_variant");
            let alias_of_variant =
                matches!(&column.type_detail, TypeDetail::Alias { base } if is_variant(base));
            !is_variant(&column.type_name) && !alias_of_variant
        }
        Dialect::Postgres => {
            let lowered = column.type_name.trim().to_ascii_lowercase();
            // An array reports `ARRAY` with its real name (`_regclass`) in the
            // detail; a materialized view's column spells it `regclass[]`.
            let element = match &column.type_detail {
                TypeDetail::Array { type_ref } => type_ref.name.trim_start_matches('_').to_string(),
                _ => lowered.trim_end_matches("[]").to_string(),
            };
            let held = match text_depends_on(column) {
                Some(KeyTextSetting::IntervalStyle) => pinned.interval_style,
                Some(KeyTextSetting::LcMonetary) => pinned.lc_monetary,
                None => true,
            };
            held && !SEARCH_PATH_DEPENDENT.contains(&element.as_str())
        }
        Dialect::Sqlite => true,
    }
}

/// What a key is judged against: the dialect, and which of the settings key
/// text depends on the connection's sessions are pinned to
/// ([`super::postgres::key_text_pins`]). From a bare [`Dialect`] every pin
/// holds — always so on SQLite and SQL Server, which read no key as server
/// text that depends on one, and on a Postgres connection that pinned both.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyRules {
    pub dialect: Dialect,
    pub pinned: KeyTextPinned,
}

impl From<Dialect> for KeyRules {
    fn from(dialect: Dialect) -> KeyRules {
        KeyRules {
            dialect,
            pinned: KeyTextPinned::ALL,
        }
    }
}

/// Which session settings every pooled session of a Postgres connection is
/// given at login. Both are `true` unless a pooler in front of the server
/// refused the startup option ([`super::postgres::open_postgres_sized`]);
/// `lc_monetary` is also `false` on an engine without the setting, which has
/// no `money` type either.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyTextPinned {
    pub interval_style: bool,
    pub lc_monetary: bool,
}

impl KeyTextPinned {
    pub const ALL: KeyTextPinned = KeyTextPinned {
        interval_style: true,
        lc_monetary: true,
    };
    pub const NONE: KeyTextPinned = KeyTextPinned {
        interval_style: false,
        lc_monetary: false,
    };
}

/// A session setting that a key's `::text` depends on and hubro pins.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyTextSetting {
    IntervalStyle,
    LcMonetary,
}

/// The pinned setting a Postgres key column's text form depends on, if any:
/// `money` (and arrays of it) prints by `lc_monetary`, arrays of `interval`
/// by `IntervalStyle`. A scalar `interval` is not here — hubro decodes it
/// from the binary value and spells it itself — and `DateStyle`, `TimeZone`
/// and `extra_float_digits` are sent by sqlx as their own startup parameters,
/// which every pooler passes on.
pub fn text_depends_on(column: &ColumnMeta) -> Option<KeyTextSetting> {
    let lowered = column.type_name.trim().to_ascii_lowercase();
    let (element, array) = match &column.type_detail {
        TypeDetail::Array { type_ref } => (type_ref.name.trim_start_matches('_').to_string(), true),
        _ => match lowered.strip_suffix("[]") {
            Some(element) => (element.trim_end().to_string(), true),
            None => (lowered.clone(), false),
        },
    };
    match element.as_str() {
        "money" => Some(KeyTextSetting::LcMonetary),
        "interval" if array => Some(KeyTextSetting::IntervalStyle),
        _ => None,
    }
}

/// The `reg*` types whose text form is qualified, or not, by `search_path` —
/// see [`key_column_addressable`].
const SEARCH_PATH_DEPENDENT: &[&str] = &[
    "regclass",
    "regcollation",
    "regconfig",
    "regdictionary",
    "regoper",
    "regoperator",
    "regproc",
    "regprocedure",
    "regtype",
];

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
            constraint: None,
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
    fn a_sql_variant_column_never_serves_as_a_key() {
        // Its values compare unequal to a value of another type family, so a
        // locator built from one finds nothing: the key is passed over, and
        // a usable unique index still stands in for it.
        let variant = |name: &str, pk: Option<u32>| ColumnMeta {
            type_name: "sql_variant".into(),
            ..col(name, false, pk)
        };
        let keyed = table(
            TableKind::Table,
            vec![variant("k", Some(1)), col("alt", false, None)],
            vec![
                index("uniq_v", true, &["k"]),
                index("uniq_alt", true, &["alt"]),
            ],
        );
        assert_eq!(
            detect_row_identity(&keyed, Dialect::SqlServer),
            Some(RowIdentity::UniqueIndex {
                name: "uniq_alt".into(),
                columns: vec!["alt".into()]
            })
        );
        let composite = table(
            TableKind::Table,
            vec![variant("k", Some(1)), col("n", false, Some(2))],
            vec![],
        );
        assert_eq!(detect_row_identity(&composite, Dialect::SqlServer), None);
        // Any other type name keys as before.
        assert!(key_column_addressable(
            &col("k", false, Some(1)),
            Dialect::SqlServer
        ));
        // An alias type is judged by the system type behind it, whatever it
        // is called.
        let aliased = |base: &str| ColumnMeta {
            type_name: "account_code".into(),
            type_detail: TypeDetail::Alias { base: base.into() },
            ..col("k", false, Some(1))
        };
        assert!(!key_column_addressable(
            &aliased("sql_variant"),
            Dialect::SqlServer
        ));
        assert!(key_column_addressable(
            &aliased("nvarchar"),
            Dialect::SqlServer
        ));
        // Only SQL Server has the type: SQLite echoes a declared
        // `sql_variant` verbatim, and on Postgres it could only be a user's
        // domain or type of that name. Both keep their identity.
        assert_eq!(
            detect_row_identity(&keyed, Dialect::Sqlite),
            Some(RowIdentity::PrimaryKey {
                columns: vec!["k".into()]
            })
        );
        assert_eq!(
            detect_row_identity(&keyed, Dialect::Postgres),
            Some(RowIdentity::PrimaryKey {
                columns: vec!["k".into()]
            })
        );
    }

    #[test]
    fn a_key_whose_text_follows_an_unpinned_setting_is_refused() {
        // When a pooler refused the startup option that pins a setting,
        // sessions may print the types depending on it differently.
        let typed = |type_name: &str, detail: TypeDetail| ColumnMeta {
            type_name: type_name.into(),
            type_detail: detail,
            ..col("k", false, Some(1))
        };
        let array_of = |name: &str| TypeDetail::Array {
            type_ref: crate::db::TypeRef {
                schema: "pg_catalog".into(),
                name: name.into(),
            },
        };
        let money = typed("money", TypeDetail::Plain);
        let monies = typed("ARRAY", array_of("_money"));
        let intervals = typed("ARRAY", array_of("_interval"));
        let interval = typed("interval", TypeDetail::Plain);
        let text = typed("text", TypeDetail::Plain);
        assert_eq!(text_depends_on(&money), Some(KeyTextSetting::LcMonetary));
        assert_eq!(text_depends_on(&monies), Some(KeyTextSetting::LcMonetary));
        assert_eq!(
            text_depends_on(&intervals),
            Some(KeyTextSetting::IntervalStyle)
        );
        assert_eq!(
            text_depends_on(&typed("interval[]", TypeDetail::Plain)),
            Some(KeyTextSetting::IntervalStyle)
        );
        // hubro spells a scalar interval itself.
        assert_eq!(text_depends_on(&interval), None);
        assert_eq!(text_depends_on(&text), None);

        let rules = |interval_style, lc_monetary| KeyRules {
            dialect: Dialect::Postgres,
            pinned: KeyTextPinned {
                interval_style,
                lc_monetary,
            },
        };
        for column in [&money, &monies, &intervals, &interval, &text] {
            assert!(
                key_column_addressable(column, rules(true, true)),
                "{column:?}"
            );
            assert!(key_column_addressable(column, Dialect::Postgres));
        }
        // PgBouncer 1.21/1.26: IntervalStyle taken, lc_monetary refused.
        assert!(!key_column_addressable(&money, rules(true, false)));
        assert!(!key_column_addressable(&monies, rules(true, false)));
        assert!(key_column_addressable(&intervals, rules(true, false)));
        // PgBouncer 1.18: no options at all.
        assert!(!key_column_addressable(&intervals, rules(false, false)));
        assert!(key_column_addressable(&interval, rules(false, false)));
        assert!(key_column_addressable(&text, rules(false, false)));
        // The identity follows: a money-keyed table reads as key-less.
        let t = table(TableKind::Table, vec![money.clone()], vec![]);
        assert_eq!(detect_row_identity(&t, rules(true, false)), None);
        assert!(detect_row_identity(&t, rules(true, true)).is_some());
    }

    #[test]
    fn a_postgres_key_whose_text_follows_search_path_is_refused() {
        // `regclass` and its kin print qualified or not by `search_path`,
        // which another pooled session may have set differently — and then
        // parse the same text as another object. `regnamespace`/`regrole`
        // print bare names whatever the path.
        let typed = |type_name: &str, detail: TypeDetail| ColumnMeta {
            type_name: type_name.into(),
            type_detail: detail,
            ..col("k", false, Some(1))
        };
        let array_of = |name: &str| TypeDetail::Array {
            type_ref: crate::db::TypeRef {
                schema: "pg_catalog".into(),
                name: name.into(),
            },
        };
        for refused in [
            typed("regclass", TypeDetail::Plain),
            typed("regtype", TypeDetail::Plain),
            typed("regprocedure", TypeDetail::Plain),
            typed("ARRAY", array_of("_regclass")),
            typed("regclass[]", TypeDetail::Plain),
        ] {
            assert!(
                !key_column_addressable(&refused, Dialect::Postgres),
                "{refused:?}"
            );
            // The name means nothing special elsewhere.
            assert!(key_column_addressable(&refused, Dialect::Sqlite));
        }
        for kept in [
            typed("regnamespace", TypeDetail::Plain),
            typed("regrole", TypeDetail::Plain),
            typed("text", TypeDetail::Plain),
            typed("ARRAY", array_of("_text")),
        ] {
            assert!(key_column_addressable(&kept, Dialect::Postgres), "{kept:?}");
        }
    }

    #[test]
    fn views_are_never_addressable() {
        // Even when introspection reports a primary key or a usable unique
        // index, a view or materialized view must stay read-only: the kind
        // check is the only thing between them and an identity. Postgres
        // never marks a matview's columns NOT NULL, so its suite cannot reach
        // this, but Materialize infers matview nullability and CockroachDB and
        // RisingWave matviews can carry keys and indexes.
        let dialects = [Dialect::Sqlite, Dialect::Postgres, Dialect::SqlServer];
        // A new dialect fails to compile here until it is added above.
        let _every_dialect = |d: Dialect| match d {
            Dialect::Sqlite | Dialect::Postgres | Dialect::SqlServer => {}
        };
        let shapes = [
            (vec![col("id", false, Some(1))], vec![]),
            (
                vec![col("id", false, None)],
                vec![index("t_id_unique", true, &["id"])],
            ),
        ];
        for (columns, indexes) in shapes {
            for dialect in dialects {
                // The control: the same shape as a table is addressable, so
                // nothing but the kind refuses it below.
                let as_table = table(TableKind::Table, columns.clone(), indexes.clone());
                assert!(
                    detect_row_identity(&as_table, dialect).is_some(),
                    "{dialect:?}: {as_table:?}"
                );
                for kind in [TableKind::View, TableKind::MaterializedView] {
                    let t = table(kind, columns.clone(), indexes.clone());
                    assert_eq!(detect_row_identity(&t, dialect), None, "{dialect:?}: {t:?}");
                }
            }
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
