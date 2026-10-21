//! Foreign-key navigation helpers: turning a source row plus one
//! [`ForeignKeyMeta`] into the [`Filter`] that pins the referenced row in the
//! target table. Pure functions so the SQL-free logic is unit-testable
//! without the Dioxus runtime.

use std::collections::HashMap;

use super::page::{Filter, KeyEquality};
use super::schema::{ForeignKeyMeta, TableMeta};
use super::sql::Dialect;
use super::staged::cast_targets;
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
/// Each term carries the referenced column's cast on `dialect`, read off
/// `target`, so the jump compares the source row's typed values the way a
/// row's own key is re-addressed — see `staged::equalities_where` for what
/// comparing them as text used to miss. The cast is the *target* column's
/// because that is the column the predicate compares against; a foreign key
/// may reference a column of a different (comparable) type.
///
/// Returns `None` — the jump is a no-op — when the source row is missing any
/// of the FK's referencing columns, when any of those values is NULL (a NULL
/// foreign key references nothing), or when a referenced column can't be
/// resolved (implicit PK reference against a target with no detectable PK).
pub fn build_fk_filter(
    fk: &ForeignKeyMeta,
    source_row: &HashMap<String, Value>,
    target: &TableMeta,
    dialect: Dialect,
) -> Option<Filter> {
    if fk.columns.is_empty() {
        return None;
    }
    let target_pk: Vec<String> = target
        .primary_key()
        .iter()
        .map(|c| c.name.clone())
        .collect();
    let casts = cast_targets(target, dialect);
    let mut terms = Vec::with_capacity(fk.columns.len());
    for (index, column) in fk.columns.iter().enumerate() {
        let value = source_row.get(column)?;
        if value.is_null() {
            return None;
        }
        let referenced = resolve_referenced_column(fk, index, &target_pk)?;
        let cast = casts.get(referenced.as_str()).cloned();
        terms.push(KeyEquality {
            column: referenced,
            value: value.clone(),
            cast,
        });
    }
    Some(Filter::Equalities(terms))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::schema::{ColumnMeta, Generated, TableKind};
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

    #[test]
    fn builds_single_column_equality() {
        let fk = make_fk(&["parent_id"], vec![Some("id")]);
        let source = row(&[("parent_id", Value::Integer(7))]);
        assert_eq!(
            build_fk_filter(&fk, &source, &by_id(), Dialect::Sqlite),
            Some(Filter::Equalities(vec![KeyEquality::uncast(
                "id",
                Value::Integer(7)
            )]))
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
            build_fk_filter(&fk, &source, &albums, Dialect::Sqlite),
            Some(Filter::Equalities(vec![
                KeyEquality::uncast("artist_id", Value::Integer(1)),
                KeyEquality::uncast("seq", Value::Integer(2)),
            ]))
        );
    }

    #[test]
    fn builds_implicit_pk_reference_using_the_target_pk() {
        let fk = make_fk(&["composer_id"], vec![None]);
        let source = row(&[("composer_id", Value::Integer(2))]);
        assert_eq!(
            build_fk_filter(&fk, &source, &by_id(), Dialect::Sqlite),
            Some(Filter::Equalities(vec![KeyEquality::uncast(
                "id",
                Value::Integer(2)
            )]))
        );
        // A target with no primary key leaves the reference unresolvable.
        let keyless = target(&[("id", "integer", None)]);
        assert_eq!(
            build_fk_filter(&fk, &source, &keyless, Dialect::Sqlite),
            None
        );
    }

    #[test]
    fn each_term_carries_the_referenced_columns_cast() {
        // The predicate compares against the target's column, so the cast is
        // the target's — here a `real` source referencing `double precision`
        // and a padded `character(3)` key, which only `bpchar` compares
        // blank-insensitively.
        let fk = make_fk(&["ratio", "code"], vec![Some("r"), Some("c")]);
        let source = row(&[
            ("ratio", Value::Real(0.5)),
            ("code", Value::Text("ab ".into())),
        ]);
        let parent = target(&[
            ("r", "double precision", Some(1)),
            ("c", "character", Some(2)),
        ]);
        let Some(Filter::Equalities(terms)) =
            build_fk_filter(&fk, &source, &parent, Dialect::Postgres)
        else {
            panic!("a jump");
        };
        let casts: Vec<Option<&str>> = terms.iter().map(|t| t.cast.as_deref()).collect();
        assert_eq!(casts, [Some("double precision"), Some("bpchar")]);
        assert_eq!(terms[0].value, Value::Real(0.5));
        // Dialects that cast nothing bind the values bare.
        for dialect in [Dialect::Sqlite, Dialect::SqlServer] {
            let Some(Filter::Equalities(terms)) = build_fk_filter(&fk, &source, &parent, dialect)
            else {
                panic!("a jump");
            };
            assert!(terms.iter().all(|t| t.cast.is_none()), "{dialect:?}");
        }
    }

    #[test]
    fn null_fk_value_yields_no_jump() {
        let fk = make_fk(&["composer_id"], vec![None]);
        let source = row(&[("composer_id", Value::Null)]);
        assert_eq!(
            build_fk_filter(&fk, &source, &by_id(), Dialect::Sqlite),
            None
        );
    }

    #[test]
    fn partial_null_in_composite_fk_yields_no_jump() {
        let fk = make_fk(&["a_id", "b_id"], vec![Some("a"), Some("b")]);
        let source = row(&[("a_id", Value::Integer(1)), ("b_id", Value::Null)]);
        let parent = target(&[("a", "integer", Some(1)), ("b", "integer", Some(2))]);
        assert_eq!(
            build_fk_filter(&fk, &source, &parent, Dialect::Sqlite),
            None
        );
    }

    #[test]
    fn missing_source_column_yields_no_jump() {
        let fk = make_fk(&["parent_id"], vec![Some("id")]);
        let source = row(&[("other", Value::Integer(1))]);
        assert_eq!(
            build_fk_filter(&fk, &source, &by_id(), Dialect::Sqlite),
            None
        );
    }
}
