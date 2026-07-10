//! Staged edits (FRE-14): pending cell updates, row inserts, and row
//! deletes, applied together in ONE transaction.
//!
//! The UI accumulates [`StagedChange`]s per table view (`ui::stage`) and
//! hands the normalized list to [`apply_staged`], which turns it into
//! guarded statements — every UPDATE/DELETE must affect exactly one row —
//! and runs them all inside a single transaction via
//! [`DbPool::execute_all_checked`]. Any failure (SQL error or row-count
//! mismatch) rolls the whole batch back and reports *which* change failed.
//!
//! Row addressing builds on the FRE-26 row-identity model
//! ([`RowIdentity`](super::rowkey::RowIdentity)); the SQL builders here are
//! value-aware siblings of `rowkey::update_sql`/`delete_sql` — they must
//! know the values (not just the columns) so NULLs can be rendered inline
//! (see [`ParamSql::value_sql`]).

use std::fmt;
use std::fmt::Write as _;

use super::page::{quote_ident, Dialect};
use super::registry::DbPool;
use super::rowkey::RowIdentity;
use super::schema::TableMeta;
use super::value::Value;

/// The identity-column values addressing one row. Value order matches
/// [`RowIdentity::key_columns`] of the table's identity.
#[derive(Debug, Clone, PartialEq)]
pub struct RowLocator {
    pub identity_values: Vec<Value>,
}

impl RowLocator {
    /// Stable, collision-free string key so rows can be used as `HashMap`
    /// keys. [`Value`] holds `f64` and therefore cannot implement
    /// `Eq`/`Hash` itself; instead rows are keyed on this serialized form.
    /// Tradeoffs, all deliberate:
    ///
    /// - variants are tagged (`n`/`i`/`r`/`t`/`b`), so `Integer(1)`,
    ///   `Text("1")`, and `Real(1.0)` never collide;
    /// - text and blobs are length-prefixed, so embedded separators cannot
    ///   splice two values into one;
    /// - reals key on their IEEE-754 bit pattern: `0.0` and `-0.0` are
    ///   *different* keys and NaN equals itself. For re-addressing a row
    ///   that was just fetched, bit-exactness is the right semantics —
    ///   numeric equality is not needed.
    pub fn key(&self) -> String {
        let mut out = String::new();
        for value in &self.identity_values {
            match value {
                Value::Null => out.push_str("n;"),
                Value::Integer(i) => {
                    let _ = write!(out, "i{i};");
                }
                Value::Real(r) => {
                    let _ = write!(out, "r{:x};", r.to_bits());
                }
                Value::Text(t) => {
                    let _ = write!(out, "t{}:{t};", t.len());
                }
                Value::Blob(b) => {
                    let _ = write!(out, "b{}:", b.len());
                    for byte in b {
                        let _ = write!(out, "{byte:02x}");
                    }
                    out.push(';');
                }
            }
        }
        out
    }

    /// Human-readable `(v1, v2)` form for messages.
    fn summary(&self) -> String {
        let parts: Vec<String> = self.identity_values.iter().map(Value::display).collect();
        format!("({})", parts.join(", "))
    }
}

/// One pending change against one table.
///
/// The model layer coalesces cell edits per `(row, column)` (last one wins)
/// before building the change list, so `apply_staged` may assume at most one
/// `Update` per row-and-column pair — duplicates would produce an invalid
/// multiple-assignment UPDATE.
#[derive(Debug, Clone, PartialEq)]
pub enum StagedChange {
    /// Set one column of one existing row.
    Update {
        locator: RowLocator,
        column: String,
        value: Value,
    },
    /// Insert one new row (`columns` empty ⇒ `INSERT … DEFAULT VALUES`).
    Insert {
        columns: Vec<String>,
        values: Vec<Value>,
    },
    /// Delete one existing row.
    Delete { locator: RowLocator },
}

impl StagedChange {
    /// Short description for error reporting ("which change failed").
    pub fn describe(&self) -> String {
        match self {
            StagedChange::Update {
                locator, column, ..
            } => format!("update of \"{column}\" for row {}", locator.summary()),
            StagedChange::Insert { .. } => "insert of a new row".to_string(),
            StagedChange::Delete { locator } => format!("delete of row {}", locator.summary()),
        }
    }
}

/// What a successful [`apply_staged`] committed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct AppliedCounts {
    /// Rows updated (several column edits on one row count as one row).
    pub updated_rows: usize,
    pub inserted_rows: usize,
    pub deleted_rows: usize,
}

/// A failed [`apply_staged`]: everything was rolled back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StagedError {
    /// Index into the caller's change list identifying the change that
    /// failed. For an UPDATE statement covering several column edits of one
    /// row, this is the index of the *first* of those edits. `None` when
    /// the failure was not attributable to a single change (opening or
    /// committing the transaction failed).
    pub change_index: Option<usize>,
    pub message: String,
}

impl fmt::Display for StagedError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.change_index {
            Some(index) => write!(f, "change {} failed: {}", index + 1, self.message),
            None => write!(f, "{}", self.message),
        }
    }
}

impl std::error::Error for StagedError {}

/// One parameterized statement plus the row count it must affect, for
/// [`DbPool::execute_all_checked`].
#[derive(Debug, Clone, PartialEq)]
pub struct CheckedStatement {
    pub sql: String,
    pub params: Vec<Value>,
    pub expected_rows: u64,
}

/// Applies all `changes` to `table` in ONE transaction. On success the
/// counts are returned; on any failure everything is rolled back and the
/// returned [`StagedError`] names the failing change by index into
/// `changes`.
///
/// Statement building:
/// - Updates are grouped per row: all `Update`s sharing a locator become a
///   single `UPDATE … SET a = …, b = … WHERE <full key>` at the position of
///   the row's first update. Callers are expected to pass the normalized
///   order (updates grouped by row, then inserts, then deletes — see
///   `ui::stage::TableStage::changes`), but any order works; grouping is by
///   locator equality, not adjacency.
/// - Every UPDATE and DELETE is guarded: affecting anything other than
///   exactly one row aborts and rolls back the whole batch (the FRE-26
///   safety net, batch-wide).
/// - NULL values are rendered inline as literal `NULL` (never as bound
///   parameters) in SET lists and INSERT values — see [`ParamSql::value_sql`].
pub async fn apply_staged(
    pool: &DbPool,
    table: &TableMeta,
    identity: &RowIdentity,
    changes: &[StagedChange],
) -> Result<AppliedCounts, StagedError> {
    if changes.is_empty() {
        return Ok(AppliedCounts::default());
    }
    let plan = build_statements(table, identity, pool.dialect(), changes)?;
    let statements: Vec<CheckedStatement> = plan.iter().map(|s| s.statement.clone()).collect();
    pool.execute_all_checked(&statements)
        .await
        .map_err(|(statement_index, error)| StagedError {
            change_index: statement_index.map(|i| plan[i].change_index),
            message: error.to_string(),
        })?;
    let mut counts = AppliedCounts::default();
    for built in &plan {
        match built.kind {
            StatementKind::Update => counts.updated_rows += 1,
            StatementKind::Insert => counts.inserted_rows += 1,
            StatementKind::Delete => counts.deleted_rows += 1,
        }
    }
    Ok(counts)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StatementKind {
    Update,
    Insert,
    Delete,
}

/// One statement of the plan, remembering which change it came from (for
/// grouped updates: the first of the row's changes).
#[derive(Debug)]
struct BuiltStatement {
    statement: CheckedStatement,
    change_index: usize,
    kind: StatementKind,
}

/// Pre-grouping slot: updates accumulate SET entries until rendered.
enum Slot {
    UpdateGroup {
        locator: RowLocator,
        first_index: usize,
        sets: Vec<(String, Value)>,
    },
    Ready(BuiltStatement),
}

/// Turns the change list into concrete statements, in first-occurrence
/// order (per-row updates collapse into the position of the row's first
/// update). Validation errors (locator arity, insert arity) are reported
/// with the offending change's index before anything touches the database.
fn build_statements(
    table: &TableMeta,
    identity: &RowIdentity,
    dialect: Dialect,
    changes: &[StagedChange],
) -> Result<Vec<BuiltStatement>, StagedError> {
    let key_len = identity.key_columns().len();
    let check_locator = |locator: &RowLocator, index: usize| {
        if locator.identity_values.len() == key_len {
            Ok(())
        } else {
            Err(StagedError {
                change_index: Some(index),
                message: format!(
                    "row locator carries {} values but the row identity needs {key_len}",
                    locator.identity_values.len()
                ),
            })
        }
    };
    let mut slots: Vec<Slot> = Vec::new();
    for (index, change) in changes.iter().enumerate() {
        match change {
            StagedChange::Update {
                locator,
                column,
                value,
            } => {
                check_locator(locator, index)?;
                let existing = slots.iter_mut().find_map(|slot| match slot {
                    Slot::UpdateGroup {
                        locator: l, sets, ..
                    } if l == locator => Some(sets),
                    _ => None,
                });
                match existing {
                    Some(sets) => sets.push((column.clone(), value.clone())),
                    None => slots.push(Slot::UpdateGroup {
                        locator: locator.clone(),
                        first_index: index,
                        sets: vec![(column.clone(), value.clone())],
                    }),
                }
            }
            StagedChange::Insert { columns, values } => {
                if columns.len() != values.len() {
                    return Err(StagedError {
                        change_index: Some(index),
                        message: format!(
                            "insert names {} columns but carries {} values",
                            columns.len(),
                            values.len()
                        ),
                    });
                }
                slots.push(Slot::Ready(BuiltStatement {
                    statement: insert_statement(table, dialect, columns, values),
                    change_index: index,
                    kind: StatementKind::Insert,
                }));
            }
            StagedChange::Delete { locator } => {
                check_locator(locator, index)?;
                slots.push(Slot::Ready(BuiltStatement {
                    statement: delete_statement(table, identity, dialect, locator),
                    change_index: index,
                    kind: StatementKind::Delete,
                }));
            }
        }
    }
    Ok(slots
        .into_iter()
        .map(|slot| match slot {
            Slot::UpdateGroup {
                locator,
                first_index,
                sets,
            } => BuiltStatement {
                statement: update_statement(table, identity, dialect, &locator, &sets),
                change_index: first_index,
                kind: StatementKind::Update,
            },
            Slot::Ready(built) => built,
        })
        .collect())
}

/// Accumulates bound parameters while rendering value positions into SQL.
struct ParamSql {
    dialect: Dialect,
    values: Vec<Value>,
}

impl ParamSql {
    fn new(dialect: Dialect) -> Self {
        ParamSql {
            dialect,
            values: Vec::new(),
        }
    }

    /// A bound placeholder for `value` — except NULL, which is rendered as
    /// the literal `NULL` (safe: not user text). Binding NULLs would break
    /// on Postgres: the drivers bind `Value::Null` as a NULL *of type text*
    /// (see `postgres::bind_params`), and Postgres rejects a text NULL for
    /// e.g. `SET int_col = $1`. A literal `NULL` has no type until the
    /// column gives it one, so `SET int_col = NULL` and
    /// `INSERT … VALUES (NULL, …)` just work on both backends.
    ///
    /// The same rendering applies in WHERE key clauses: `col = NULL` is
    /// never true, so a NULL identity value matches nothing and the
    /// row-count guard aborts the batch — the safe outcome, since a NULL
    /// key cannot address a row anyway.
    fn value_sql(&mut self, value: &Value) -> String {
        if value.is_null() {
            return "NULL".to_string();
        }
        self.values.push(value.clone());
        match self.dialect {
            Dialect::Sqlite => "?".to_string(),
            Dialect::Postgres => format!("${}", self.values.len()),
        }
    }
}

/// `UPDATE t SET a = ?, b = NULL WHERE k1 = ? AND k2 = ?`, guarded to one
/// row.
fn update_statement(
    table: &TableMeta,
    identity: &RowIdentity,
    dialect: Dialect,
    locator: &RowLocator,
    sets: &[(String, Value)],
) -> CheckedStatement {
    debug_assert!(!sets.is_empty(), "UPDATE needs at least one SET");
    let mut params = ParamSql::new(dialect);
    let assignments: Vec<String> = sets
        .iter()
        .map(|(column, value)| format!("{} = {}", quote_ident(column), params.value_sql(value)))
        .collect();
    let sql = format!(
        "UPDATE {} SET {} WHERE {}",
        qualified_table(table),
        assignments.join(", "),
        key_clause(identity, locator, &mut params),
    );
    CheckedStatement {
        sql,
        params: params.values,
        expected_rows: 1,
    }
}

/// `INSERT INTO t ("a", "b") VALUES (?, NULL)`, or `DEFAULT VALUES` for an
/// all-defaults row. Guarded to one row (inserts always affect exactly one).
fn insert_statement(
    table: &TableMeta,
    dialect: Dialect,
    columns: &[String],
    values: &[Value],
) -> CheckedStatement {
    let mut params = ParamSql::new(dialect);
    let sql = if columns.is_empty() {
        format!("INSERT INTO {} DEFAULT VALUES", qualified_table(table))
    } else {
        let names: Vec<String> = columns.iter().map(|c| quote_ident(c)).collect();
        let rendered: Vec<String> = values.iter().map(|v| params.value_sql(v)).collect();
        format!(
            "INSERT INTO {} ({}) VALUES ({})",
            qualified_table(table),
            names.join(", "),
            rendered.join(", "),
        )
    };
    CheckedStatement {
        sql,
        params: params.values,
        expected_rows: 1,
    }
}

/// `DELETE FROM t WHERE k1 = ? AND k2 = ?`, guarded to one row.
fn delete_statement(
    table: &TableMeta,
    identity: &RowIdentity,
    dialect: Dialect,
    locator: &RowLocator,
) -> CheckedStatement {
    let mut params = ParamSql::new(dialect);
    let sql = format!(
        "DELETE FROM {} WHERE {}",
        qualified_table(table),
        key_clause(identity, locator, &mut params),
    );
    CheckedStatement {
        sql,
        params: params.values,
        expected_rows: 1,
    }
}

/// `"k1" = ? AND "k2" = NULL` over the full key, pairing the identity's key
/// columns with the locator's values (arity is validated by the caller).
fn key_clause(identity: &RowIdentity, locator: &RowLocator, params: &mut ParamSql) -> String {
    identity
        .key_columns()
        .iter()
        .zip(&locator.identity_values)
        .map(|(column, value)| format!("{} = {}", quote_ident(column), params.value_sql(value)))
        .collect::<Vec<_>>()
        .join(" AND ")
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
    use crate::db::schema::{ColumnMeta, TableKind};

    fn col(name: &str, pk: Option<u32>) -> ColumnMeta {
        ColumnMeta {
            name: name.into(),
            type_name: "TEXT".into(),
            nullable: pk.is_none(),
            primary_key_position: pk,
            default: None,
        }
    }

    fn table() -> TableMeta {
        TableMeta {
            schema: None,
            name: "t".into(),
            kind: TableKind::Table,
            columns: vec![col("id", Some(1)), col("a", None), col("b", None)],
            indexes: vec![],
            foreign_keys: vec![],
        }
    }

    fn pg_table() -> TableMeta {
        TableMeta {
            schema: Some("app".into()),
            ..table()
        }
    }

    fn identity() -> RowIdentity {
        RowIdentity::PrimaryKey {
            columns: vec!["id".into()],
        }
    }

    fn locator(id: i64) -> RowLocator {
        RowLocator {
            identity_values: vec![Value::Integer(id)],
        }
    }

    fn update(id: i64, column: &str, value: Value) -> StagedChange {
        StagedChange::Update {
            locator: locator(id),
            column: column.into(),
            value,
        }
    }

    #[test]
    fn locator_keys_distinguish_types_and_are_splice_proof() {
        let key = |values: Vec<Value>| {
            RowLocator {
                identity_values: values,
            }
            .key()
        };
        // Same digits, different storage classes: distinct keys.
        assert_ne!(
            key(vec![Value::Integer(1)]),
            key(vec![Value::Text("1".into())])
        );
        assert_ne!(key(vec![Value::Integer(1)]), key(vec![Value::Real(1.0)]));
        // NULL is not the text "NULL".
        assert_ne!(
            key(vec![Value::Null]),
            key(vec![Value::Text("NULL".into())])
        );
        // Text containing the separator cannot splice into two values.
        assert_ne!(
            key(vec![Value::Text("a;t1:b".into())]),
            key(vec![Value::Text("a".into()), Value::Text("b".into())])
        );
        // Blob vs text with the same bytes: distinct.
        assert_ne!(
            key(vec![Value::Blob(b"ab".to_vec())]),
            key(vec![Value::Text("ab".into())])
        );
        // Reals key on bits: -0.0 != 0.0, NaN equals itself.
        assert_ne!(key(vec![Value::Real(0.0)]), key(vec![Value::Real(-0.0)]));
        assert_eq!(
            key(vec![Value::Real(f64::NAN)]),
            key(vec![Value::Real(f64::NAN)])
        );
        // Equal composite locators produce equal keys.
        assert_eq!(
            key(vec![Value::Integer(7), Value::Text("x".into())]),
            key(vec![Value::Integer(7), Value::Text("x".into())])
        );
    }

    #[test]
    fn updates_on_one_row_group_into_a_single_multi_column_statement() {
        let plan = build_statements(
            &table(),
            &identity(),
            Dialect::Sqlite,
            &[
                update(1, "a", Value::Text("x".into())),
                update(1, "b", Value::Integer(2)),
            ],
        )
        .unwrap();
        assert_eq!(plan.len(), 1);
        assert_eq!(
            plan[0].statement.sql,
            "UPDATE \"t\" SET \"a\" = ?, \"b\" = ? WHERE \"id\" = ?"
        );
        assert_eq!(
            plan[0].statement.params,
            vec![
                Value::Text("x".into()),
                Value::Integer(2),
                Value::Integer(1)
            ]
        );
        assert_eq!(plan[0].statement.expected_rows, 1);
        assert_eq!(plan[0].change_index, 0);
    }

    #[test]
    fn grouping_is_by_locator_not_adjacency_and_attributes_the_first_change() {
        let plan = build_statements(
            &table(),
            &identity(),
            Dialect::Sqlite,
            &[
                update(1, "a", Value::Integer(10)),
                update(2, "a", Value::Integer(20)),
                update(1, "b", Value::Integer(11)),
            ],
        )
        .unwrap();
        // Two statements: row 1 (changes 0 and 2, grouped, attributed to 0)
        // and row 2 (change 1).
        assert_eq!(plan.len(), 2);
        assert_eq!(plan[0].change_index, 0);
        assert!(plan[0].statement.sql.contains("\"a\" = ?, \"b\" = ?"));
        assert_eq!(plan[1].change_index, 1);
    }

    #[test]
    fn null_values_render_as_literal_null_never_as_parameters() {
        // UPDATE: SET NULL inline; only the non-NULL set value and the key
        // are bound. This is what makes `SET int_col = NULL` work on
        // Postgres, where a bound Value::Null is typed as text.
        let plan = build_statements(
            &pg_table(),
            &identity(),
            Dialect::Postgres,
            &[
                update(1, "a", Value::Null),
                update(1, "b", Value::Text("x".into())),
            ],
        )
        .unwrap();
        assert_eq!(
            plan[0].statement.sql,
            "UPDATE \"app\".\"t\" SET \"a\" = NULL, \"b\" = $1 WHERE \"id\" = $2"
        );
        assert_eq!(
            plan[0].statement.params,
            vec![Value::Text("x".into()), Value::Integer(1)]
        );

        // INSERT: NULL inline in VALUES.
        let plan = build_statements(
            &pg_table(),
            &identity(),
            Dialect::Postgres,
            &[StagedChange::Insert {
                columns: vec!["id".into(), "a".into(), "b".into()],
                values: vec![Value::Integer(5), Value::Null, Value::Text("y".into())],
            }],
        )
        .unwrap();
        assert_eq!(
            plan[0].statement.sql,
            "INSERT INTO \"app\".\"t\" (\"id\", \"a\", \"b\") VALUES ($1, NULL, $2)"
        );
        assert_eq!(
            plan[0].statement.params,
            vec![Value::Integer(5), Value::Text("y".into())]
        );

        // WHERE: a NULL key value renders inline too; `col = NULL` matches
        // nothing, so the row-count guard aborts instead of erroring on a
        // typed-NULL bind.
        let plan = build_statements(
            &pg_table(),
            &identity(),
            Dialect::Postgres,
            &[StagedChange::Delete {
                locator: RowLocator {
                    identity_values: vec![Value::Null],
                },
            }],
        )
        .unwrap();
        assert_eq!(
            plan[0].statement.sql,
            "DELETE FROM \"app\".\"t\" WHERE \"id\" = NULL"
        );
        assert!(plan[0].statement.params.is_empty());
    }

    #[test]
    fn plan_preserves_first_occurrence_order_across_kinds() {
        let plan = build_statements(
            &table(),
            &identity(),
            Dialect::Sqlite,
            &[
                update(1, "a", Value::Integer(10)),
                StagedChange::Insert {
                    columns: vec!["a".into()],
                    values: vec![Value::Integer(1)],
                },
                StagedChange::Delete {
                    locator: locator(2),
                },
            ],
        )
        .unwrap();
        assert_eq!(
            plan.iter().map(|s| s.kind).collect::<Vec<_>>(),
            [
                StatementKind::Update,
                StatementKind::Insert,
                StatementKind::Delete
            ]
        );
        assert_eq!(
            plan.iter().map(|s| s.change_index).collect::<Vec<_>>(),
            [0, 1, 2]
        );
    }

    #[test]
    fn empty_column_insert_uses_default_values() {
        let plan = build_statements(
            &table(),
            &identity(),
            Dialect::Sqlite,
            &[StagedChange::Insert {
                columns: vec![],
                values: vec![],
            }],
        )
        .unwrap();
        assert_eq!(plan[0].statement.sql, "INSERT INTO \"t\" DEFAULT VALUES");
    }

    #[test]
    fn composite_key_delete_targets_the_full_key() {
        let composite = RowIdentity::PrimaryKey {
            columns: vec!["k1".into(), "k2".into()],
        };
        let plan = build_statements(
            &table(),
            &composite,
            Dialect::Postgres,
            &[StagedChange::Delete {
                locator: RowLocator {
                    identity_values: vec![Value::Integer(1), Value::Text("x".into())],
                },
            }],
        )
        .unwrap();
        assert_eq!(
            plan[0].statement.sql,
            "DELETE FROM \"t\" WHERE \"k1\" = $1 AND \"k2\" = $2"
        );
    }

    #[test]
    fn arity_mismatches_are_rejected_with_the_offending_index() {
        let short_locator = StagedChange::Delete {
            locator: RowLocator {
                identity_values: vec![],
            },
        };
        let err = build_statements(
            &table(),
            &identity(),
            Dialect::Sqlite,
            &[update(1, "a", Value::Integer(1)), short_locator],
        )
        .unwrap_err();
        assert_eq!(err.change_index, Some(1));
        assert!(err.message.contains("locator"), "message: {}", err.message);

        let bad_insert = StagedChange::Insert {
            columns: vec!["a".into(), "b".into()],
            values: vec![Value::Integer(1)],
        };
        let err =
            build_statements(&table(), &identity(), Dialect::Sqlite, &[bad_insert]).unwrap_err();
        assert_eq!(err.change_index, Some(0));
        assert!(
            err.message.contains("2 columns"),
            "message: {}",
            err.message
        );
    }

    #[test]
    fn describe_names_the_change() {
        assert_eq!(
            update(7, "title", Value::Null).describe(),
            "update of \"title\" for row (7)"
        );
        assert_eq!(
            StagedChange::Delete {
                locator: RowLocator {
                    identity_values: vec![Value::Integer(1), Value::Text("x".into())],
                },
            }
            .describe(),
            "delete of row (1, x)"
        );
        assert_eq!(
            StagedChange::Insert {
                columns: vec![],
                values: vec![],
            }
            .describe(),
            "insert of a new row"
        );
    }

    #[test]
    fn staged_error_display_is_one_based() {
        let err = StagedError {
            change_index: Some(2),
            message: "boom".into(),
        };
        assert_eq!(err.to_string(), "change 3 failed: boom");
        let err = StagedError {
            change_index: None,
            message: "boom".into(),
        };
        assert_eq!(err.to_string(), "boom");
    }
}
