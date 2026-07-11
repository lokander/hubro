//! Foreign-key navigation helpers (FRE-29): turning a source row plus one
//! [`ForeignKeyMeta`] into the [`Filter`] that pins the referenced row in the
//! target table. Pure functions so the SQL-free logic is unit-testable
//! without the Dioxus runtime.

use std::collections::HashMap;

use super::page::Filter;
use super::schema::ForeignKeyMeta;
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

/// Builds the multi-equality [`Filter`] that selects the row `fk` references,
/// from the source row's values (column name → value).
///
/// Returns `None` — the jump is a no-op — when the source row is missing any
/// of the FK's referencing columns, when any of those values is NULL (a NULL
/// foreign key references nothing), or when a referenced column can't be
/// resolved (implicit PK reference against a target with no detectable PK).
pub fn build_fk_filter(
    fk: &ForeignKeyMeta,
    source_row: &HashMap<String, Value>,
    target_pk: &[String],
) -> Option<Filter> {
    if fk.columns.is_empty() {
        return None;
    }
    let mut pairs = Vec::with_capacity(fk.columns.len());
    for (index, column) in fk.columns.iter().enumerate() {
        let value = source_row.get(column)?;
        if value.is_null() {
            return None;
        }
        let referenced = resolve_referenced_column(fk, index, target_pk)?;
        pairs.push((referenced, value.clone()));
    }
    Some(Filter::Equalities(pairs))
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn builds_single_column_equality() {
        let fk = make_fk(&["parent_id"], vec![Some("id")]);
        let source = row(&[("parent_id", Value::Integer(7))]);
        assert_eq!(
            build_fk_filter(&fk, &source, &[]),
            Some(Filter::Equalities(vec![("id".into(), Value::Integer(7))]))
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
        assert_eq!(
            build_fk_filter(&fk, &source, &[]),
            Some(Filter::Equalities(vec![
                ("artist_id".into(), Value::Integer(1)),
                ("seq".into(), Value::Integer(2)),
            ]))
        );
    }

    #[test]
    fn builds_implicit_pk_reference_using_the_target_pk() {
        let fk = make_fk(&["composer_id"], vec![None]);
        let source = row(&[("composer_id", Value::Integer(2))]);
        let pk = vec!["id".to_string()];
        assert_eq!(
            build_fk_filter(&fk, &source, &pk),
            Some(Filter::Equalities(vec![("id".into(), Value::Integer(2))]))
        );
    }

    #[test]
    fn null_fk_value_yields_no_jump() {
        let fk = make_fk(&["composer_id"], vec![None]);
        let source = row(&[("composer_id", Value::Null)]);
        assert_eq!(build_fk_filter(&fk, &source, &["id".to_string()]), None);
    }

    #[test]
    fn partial_null_in_composite_fk_yields_no_jump() {
        let fk = make_fk(&["a_id", "b_id"], vec![Some("a"), Some("b")]);
        let source = row(&[("a_id", Value::Integer(1)), ("b_id", Value::Null)]);
        assert_eq!(build_fk_filter(&fk, &source, &[]), None);
    }

    #[test]
    fn missing_source_column_yields_no_jump() {
        let fk = make_fk(&["parent_id"], vec![Some("id")]);
        let source = row(&[("other", Value::Integer(1))]);
        assert_eq!(build_fk_filter(&fk, &source, &[]), None);
    }
}
