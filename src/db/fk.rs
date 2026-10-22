//! Foreign-key navigation helpers: turning a source row plus one
//! [`ForeignKeyMeta`] into the [`Filter`] that pins the referenced row in the
//! target table. Pure functions so the SQL-free logic is unit-testable
//! without the Dioxus runtime.

use std::collections::HashMap;

use super::page::{Filter, KeyEquality};
use super::rowkey::{key_column_addressable, KeyRules};
use super::schema::{ColumnMeta, ForeignKeyMeta, TableMeta};
use super::value::Value;

/// The name of the target column the FK's `index`-th key column references.
///
/// `referenced_columns[index]` is `Some(name)` for an explicit reference and
/// `None` when the FK points at the target table's implicit primary key — in
/// which case the name is the `index`-th primary-key column, in key order
/// (`target_pk`). Returns `None` when the reference can't be resolved (an
/// implicit reference with no known PK column at that position).
pub fn resolve_referenced_column(
    fk: &ForeignKeyMeta,
    index: usize,
    target_pk: &[String],
) -> Option<String> {
    match fk.referenced_columns.get(index)? {
        Some(name) => Some(name.clone()),
        None => target_pk.get(index).cloned(),
    }
}

/// Builds the multi-equality [`Filter`] that selects the row `fk` references
/// in `target`, from the source row's values (column name → value).
///
/// Each term carries the referenced column's metadata, read off `target`, so
/// the jump binds the source row's typed values by the column they are
/// compared against, the way a row's own key is re-addressed — see
/// `staged::equalities_where` for what comparing them as text used to miss.
/// It is the *target* column's because that is the column the predicate
/// compares against; a foreign key may reference a column of a different
/// (comparable) type.
///
/// Returns `None` — the jump is a no-op — when the source row is missing any
/// of the FK's referencing columns, when any of those values is NULL (a NULL
/// foreign key references nothing), or when the jump has nowhere to land
/// ([`fk_target_columns`]).
pub fn build_fk_filter(
    fk: &ForeignKeyMeta,
    source_row: &HashMap<String, Value>,
    target: &TableMeta,
    rules: impl Into<KeyRules>,
) -> Option<Filter> {
    let referenced = fk_target_columns(fk, target, rules)?;
    let mut terms = Vec::with_capacity(referenced.len());
    for (column, meta) in fk.columns.iter().zip(referenced) {
        let value = source_row.get(column)?;
        if value.is_null() {
            return None;
        }
        terms.push(KeyEquality {
            column: meta.name.clone(),
            value: value.clone(),
            target: Some(meta.clone()),
        });
    }
    Some(Filter::Equalities(terms))
}

/// The columns of `target` that `fk`'s key columns reference, in key order —
/// or `None` when the foreign key leads nowhere hubro can show: it has no
/// columns, a reference to the implicit primary key finds no primary-key
/// column at its position, a referenced column is missing from `target`
/// (a stale schema, or a reference SQLite accepted to a column that does
/// not exist), or a referenced column's values cannot be bound back to find
/// their row under `rules` ([`key_column_addressable`]: a jump on a SQL Server
/// `sql_variant` key landed on an empty page). Whether a row's link is offered
/// at all is decided by this, so a link is never a dead click.
pub fn fk_target_columns<'a>(
    fk: &ForeignKeyMeta,
    target: &'a TableMeta,
    rules: impl Into<KeyRules>,
) -> Option<Vec<&'a ColumnMeta>> {
    let rules = rules.into();
    if fk.columns.is_empty() {
        return None;
    }
    let target_pk: Vec<String> = target
        .primary_key()
        .iter()
        .map(|c| c.name.clone())
        .collect();
    (0..fk.columns.len())
        .map(|index| {
            let name = resolve_referenced_column(fk, index, &target_pk)?;
            target
                .columns
                .iter()
                .find(|c| c.name == name)
                .filter(|c| key_column_addressable(c, rules))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::schema::{ColumnMeta, Generated, TableKind};
    use crate::db::sql::Dialect;
    use crate::db::TypeDetail;

    fn make_fk(columns: &[&str], referenced: Vec<Option<&str>>) -> ForeignKeyMeta {
        ForeignKeyMeta {
            columns: columns.iter().map(|c| c.to_string()).collect(),
            referenced_schema: None,
            referenced_table: "parent".into(),
            referenced_columns: referenced
                .into_iter()
                .map(|r| r.map(str::to_string))
                .collect(),
        }
    }

    fn row(pairs: &[(&str, Value)]) -> HashMap<String, Value> {
        pairs
            .iter()
            .map(|(c, v)| (c.to_string(), v.clone()))
            .collect()
    }

    #[test]
    fn resolves_explicit_reference() {
        let fk = make_fk(&["parent_id"], vec![Some("id")]);
        assert_eq!(
            resolve_referenced_column(&fk, 0, &[]),
            Some("id".to_string())
        );
    }

    #[test]
    fn resolves_implicit_reference_from_the_target_pk() {
        // `REFERENCES parent` with no column list → the target's PK column.
        let fk = make_fk(&["parent_id"], vec![None]);
        let pk = vec!["id".to_string()];
        assert_eq!(
            resolve_referenced_column(&fk, 0, &pk),
            Some("id".to_string())
        );
        // Composite implicit reference maps position-for-position.
        let fk2 = make_fk(&["a_id", "b_id"], vec![None, None]);
        let pk2 = vec!["a".to_string(), "b".to_string()];
        assert_eq!(
            resolve_referenced_column(&fk2, 1, &pk2),
            Some("b".to_string())
        );
    }

    #[test]
    fn implicit_reference_without_a_pk_is_unresolvable() {
        let fk = make_fk(&["parent_id"], vec![None]);
        assert_eq!(resolve_referenced_column(&fk, 0, &[]), None);
    }

    /// A target table whose columns are `(name, type, pk position)`.
    fn target(columns: &[(&str, &str, Option<u32>)]) -> TableMeta {
        TableMeta {
            schema: Some("app".into()),
            name: "parent".into(),
            kind: TableKind::Table,
            columns: columns
                .iter()
                .map(|(name, type_name, pk)| ColumnMeta {
                    name: name.to_string(),
                    type_name: type_name.to_string(),
                    nullable: pk.is_none(),
                    primary_key_position: *pk,
                    default: None,
                    generated: Generated::Never,
                    type_detail: TypeDetail::Plain,
                })
                .collect(),
            indexes: vec![],
            foreign_keys: vec![],
            restriction: None,
            internal: None,
            kind_label: None,
        }
    }

    fn by_id() -> TableMeta {
        target(&[("id", "integer", Some(1)), ("name", "text", None)])
    }

    /// The jump's terms as `(column, value, the column's declared type)`.
    fn jump(
        fk: &ForeignKeyMeta,
        source: &HashMap<String, Value>,
        target: &TableMeta,
    ) -> Option<Vec<(String, Value, String)>> {
        let Filter::Equalities(terms) = build_fk_filter(fk, source, target, Dialect::Postgres)?
        else {
            panic!("a jump builds an equalities filter");
        };
        Some(
            terms
                .into_iter()
                .map(|term| {
                    let meta = term.target.expect("every term is typed by its column");
                    assert_eq!(meta.name, term.column);
                    (term.column, term.value, meta.type_name)
                })
                .collect(),
        )
    }

    fn term(column: &str, value: Value, type_name: &str) -> (String, Value, String) {
        (column.into(), value, type_name.into())
    }

    #[test]
    fn builds_single_column_equality() {
        let fk = make_fk(&["parent_id"], vec![Some("id")]);
        let source = row(&[("parent_id", Value::Integer(7))]);
        assert_eq!(
            jump(&fk, &source, &by_id()),
            Some(vec![term("id", Value::Integer(7), "integer")])
        );
    }

    #[test]
    fn builds_multi_column_equality_in_key_order() {
        let fk = make_fk(
            &["album_artist_id", "album_seq"],
            vec![Some("artist_id"), Some("seq")],
        );
        let source = row(&[
            ("album_artist_id", Value::Integer(1)),
            ("album_seq", Value::Integer(2)),
            ("title", Value::Text("Opening".into())),
        ]);
        let albums = target(&[
            ("artist_id", "INTEGER", Some(1)),
            ("seq", "INTEGER", Some(2)),
        ]);
        assert_eq!(
            jump(&fk, &source, &albums),
            Some(vec![
                term("artist_id", Value::Integer(1), "INTEGER"),
                term("seq", Value::Integer(2), "INTEGER"),
            ])
        );
    }

    #[test]
    fn builds_implicit_pk_reference_using_the_target_pk() {
        let fk = make_fk(&["composer_id"], vec![None]);
        let source = row(&[("composer_id", Value::Integer(2))]);
        assert_eq!(
            jump(&fk, &source, &by_id()),
            Some(vec![term("id", Value::Integer(2), "integer")])
        );
        // A target with no primary key leaves the reference unresolvable.
        let keyless = target(&[("id", "integer", None)]);
        assert_eq!(
            build_fk_filter(&fk, &source, &keyless, Dialect::Postgres),
            None
        );
        assert_eq!(fk_target_columns(&fk, &keyless, Dialect::Postgres), None);
    }

    #[test]
    fn each_term_is_typed_by_the_referenced_column() {
        // The predicate compares against the target's column, so the type
        // is the target's — here a `real` source referencing `double
        // precision`, and a padded `character(3)` key.
        let fk = make_fk(&["ratio", "code"], vec![Some("r"), Some("c")]);
        let source = row(&[
            ("ratio", Value::Real(0.5)),
            ("code", Value::Text("ab ".into())),
        ]);
        let parent = target(&[
            ("r", "double precision", Some(1)),
            ("c", "character", Some(2)),
        ]);
        assert_eq!(
            jump(&fk, &source, &parent),
            Some(vec![
                term("r", Value::Real(0.5), "double precision"),
                term("c", Value::Text("ab ".into()), "character"),
            ])
        );
    }

    #[test]
    fn a_reference_to_a_sql_variant_key_offers_no_jump() {
        // A variant holding an `int` or a `date` matches no row when bound
        // back, so the jump would land on an empty page; it is not offered —
        // on SQL Server. Elsewhere `sql_variant` is only a name (SQLite echoes
        // a declared type verbatim), and the jump stands.
        let fk = make_fk(&["ref"], vec![Some("k")]);
        let source = row(&[("ref", Value::Text("5".into()))]);
        let parent = target(&[("k", "sql_variant", Some(1))]);
        assert_eq!(fk_target_columns(&fk, &parent, Dialect::SqlServer), None);
        assert_eq!(
            build_fk_filter(&fk, &source, &parent, Dialect::SqlServer),
            None
        );
        for dialect in [Dialect::Sqlite, Dialect::Postgres] {
            assert!(
                fk_target_columns(&fk, &parent, dialect).is_some(),
                "{dialect:?}"
            );
            assert!(
                build_fk_filter(&fk, &source, &parent, dialect).is_some(),
                "{dialect:?}"
            );
        }
    }

    #[test]
    fn a_reference_to_a_column_the_target_lacks_leads_nowhere() {
        // SQLite accepts `REFERENCES parent(nope)`; so does a schema read
        // before the target changed. Either way there is nothing to land on,
        // so no link is offered and no jump is built.
        let fk = make_fk(&["parent_id"], vec![Some("nope")]);
        let source = row(&[("parent_id", Value::Integer(7))]);
        assert_eq!(fk_target_columns(&fk, &by_id(), Dialect::Postgres), None);
        assert_eq!(
            build_fk_filter(&fk, &source, &by_id(), Dialect::Postgres),
            None
        );
        let fine = make_fk(&["parent_id"], vec![Some("id")]);
        let parent = by_id();
        let resolved = fk_target_columns(&fine, &parent, Dialect::Postgres).unwrap();
        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].name, "id");
    }

    #[test]
    fn null_fk_value_yields_no_jump() {
        let fk = make_fk(&["composer_id"], vec![None]);
        let source = row(&[("composer_id", Value::Null)]);
        assert_eq!(
            build_fk_filter(&fk, &source, &by_id(), Dialect::Postgres),
            None
        );
    }

    #[test]
    fn partial_null_in_composite_fk_yields_no_jump() {
        let fk = make_fk(&["a_id", "b_id"], vec![Some("a"), Some("b")]);
        let source = row(&[("a_id", Value::Integer(1)), ("b_id", Value::Null)]);
        let parent = target(&[("a", "integer", Some(1)), ("b", "integer", Some(2))]);
        assert_eq!(
            build_fk_filter(&fk, &source, &parent, Dialect::Postgres),
            None
        );
    }

    #[test]
    fn missing_source_column_yields_no_jump() {
        let fk = make_fk(&["parent_id"], vec![Some("id")]);
        let source = row(&[("other", Value::Integer(1))]);
        assert_eq!(
            build_fk_filter(&fk, &source, &by_id(), Dialect::Postgres),
            None
        );
    }
}
