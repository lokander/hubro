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
//! value-aware — they must know the values (not just the columns) so NULLs
//! can be rendered inline (see [`ParamSql::value_sql`]). They are the only
//! UPDATE/DELETE builders in the crate: `rowkey` reports how a row is
//! addressed, this module writes it.
//!
//! On Postgres every bound parameter is cast to its column's introspected
//! type (`SET "col" = $1::integer`) so text-staged values coerce — see
//! [`cast_target`] for why and when the cast is skipped.

use std::collections::HashMap;
use std::fmt;
use std::fmt::Write as _;

use super::caps::{self, TableAccess};
use super::error::DbError;
use super::registry::DbPool;
use super::rowkey::RowIdentity;
use super::schema::{ColumnMeta, TableMeta};
use super::sql::{qualified, quote_ident, Dialect};
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

/// A failed [`apply_staged`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StagedError {
    /// Index into the caller's change list identifying the change that
    /// failed. For an UPDATE statement covering several column edits of one
    /// row, this is the index of the *first* of those edits. `None` when
    /// the failure was not attributable to a single change (opening or
    /// committing the transaction failed — in which case there is no
    /// rollback guarantee either).
    pub change_index: Option<usize>,
    /// Human-readable summary of the failing change; for a grouped UPDATE
    /// it names the row and every column it set (e.g. `update of row (1, 2)
    /// [columns title, year]`). `None` exactly when `change_index` is.
    pub change_summary: Option<String>,
    pub message: String,
}

impl fmt::Display for StagedError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match (self.change_index, &self.change_summary) {
            (Some(index), Some(summary)) => {
                write!(
                    f,
                    "change {} ({summary}) failed: {}",
                    index + 1,
                    self.message
                )
            }
            (Some(index), None) => write!(f, "change {} failed: {}", index + 1, self.message),
            _ => write!(f, "{}", self.message),
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
/// - Updates are grouped per row: all `Update`s sharing a row — matched by
///   [`RowLocator::key`], the same bit-exact identity the UI stage keys rows
///   on, NOT by `PartialEq` (so `0.0`/`-0.0` locators stay two rows and NaN
///   groups with itself) — become a single `UPDATE … SET a = …, b = …
///   WHERE <full key>` at the position of the row's first update. Callers
///   are expected to pass the normalized order (updates grouped by row,
///   then inserts, then deletes — see `ui::stage::TableStage::changes`),
///   but any order works; grouping is by row key, not adjacency.
/// - Every UPDATE and DELETE is guarded: affecting anything other than
///   exactly one row aborts and rolls back the whole batch (the FRE-26
///   safety net, batch-wide).
/// - NULL values are rendered inline as literal `NULL` (never as bound
///   parameters) in SET lists and INSERT values — see [`ParamSql::value_sql`].
///
/// `access` — the connection's capabilities resolved for `table` (FRE-87),
/// including the user's own write protection (FRE-111) — is checked first:
/// without `mutate`, nothing is built or sent and the returned
/// [`StagedError`] carries the same sentence the UI shows on the disabled
/// Save button. The UI never offers staging on such a table, so reaching
/// this means a gate was missed.
///
/// It is passed in rather than re-resolved from `pool` so that this backstop
/// and the UI's gate cannot answer differently: `pool` alone doesn't know
/// what the user marked the connection.
pub async fn apply_staged(
    pool: &DbPool,
    access: &TableAccess,
    table: &TableMeta,
    identity: &RowIdentity,
    changes: &[StagedChange],
) -> Result<AppliedCounts, StagedError> {
    if changes.is_empty() {
        return Ok(AppliedCounts::default());
    }
    if !access.can_mutate() {
        return Err(StagedError {
            change_index: None,
            change_summary: None,
            message: DbError::Unsupported(
                access
                    .read_only_notice()
                    .unwrap_or(caps::NO_MUTATE)
                    .to_string(),
            )
            .to_string(),
        });
    }
    // Statements and their metadata come back as parallel-by-index vecs so
    // the statements — which carry every staged cell value — go to
    // `execute_all_checked` as-is instead of being cloned out of a combined
    // struct (FRE-131); the metadata is only consulted on failure.
    let (statements, metas) = build_statements(table, identity, pool.dialect(), changes)?;
    pool.execute_all_checked(&statements)
        .await
        .map_err(|(statement_index, error)| StagedError {
            change_index: statement_index.map(|i| metas[i].change_index),
            change_summary: statement_index.map(|i| metas[i].summary.clone()),
            message: error.to_string(),
        })?;
    let mut counts = AppliedCounts::default();
    for meta in &metas {
        match meta.kind {
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

/// What one planned statement means, parallel by index to the statements
/// vec [`build_statements`] returns: which change it came from (for grouped
/// updates: the first of the row's changes) and how to describe it in a
/// failure message (for grouped updates: the row and ALL its columns, not
/// just the first). Kept apart from [`CheckedStatement`] so the statements
/// — which carry every staged value — need never be cloned out of a
/// combined struct just to reach the executor (FRE-131).
#[derive(Debug)]
struct StatementMeta {
    change_index: usize,
    kind: StatementKind,
    summary: String,
}

/// Pre-grouping slot: updates accumulate SET entries until rendered.
enum Slot {
    UpdateGroup {
        locator: RowLocator,
        first_index: usize,
        sets: Vec<(String, Value)>,
    },
    Ready {
        statement: CheckedStatement,
        meta: StatementMeta,
    },
}

/// Turns the change list into concrete statements plus their
/// parallel-by-index metadata, in first-occurrence order (per-row updates
/// collapse into the position of the row's first update). Validation errors
/// (locator arity, insert arity) are reported with the offending change's
/// index before anything touches the database.
fn build_statements(
    table: &TableMeta,
    identity: &RowIdentity,
    dialect: Dialect,
    changes: &[StagedChange],
) -> Result<(Vec<CheckedStatement>, Vec<StatementMeta>), StagedError> {
    let key_len = identity.key_columns().len();
    let check_locator = |locator: &RowLocator, index: usize, change: &StagedChange| {
        if locator.identity_values.len() == key_len {
            Ok(())
        } else {
            Err(StagedError {
                change_index: Some(index),
                change_summary: Some(change.describe()),
                message: format!(
                    "row locator carries {} values but the row identity needs {key_len}",
                    locator.identity_values.len()
                ),
            })
        }
    };
    let casts = cast_targets(table, dialect);
    let mut slots: Vec<Slot> = Vec::new();
    // Row key → index into `slots` of the row's UpdateGroup. The Vec keeps
    // first-occurrence order; the map makes the group lookup O(1) instead
    // of a linear scan over every prior slot, which made large staged
    // batches quadratic in string compares (FRE-131). Grouping is by the
    // row KEY ([`RowLocator::key`] — bit-exact, the identity the UI stage
    // coalesced under), unchanged: PartialEq would merge 0.0/-0.0 locators
    // (a duplicate-SET error at best, the wrong row at worst) and never
    // group NaN with itself.
    let mut update_groups: HashMap<String, usize> = HashMap::new();
    for (index, change) in changes.iter().enumerate() {
        match change {
            StagedChange::Update {
                locator,
                column,
                value,
            } => {
                check_locator(locator, index, change)?;
                match update_groups.entry(locator.key()) {
                    std::collections::hash_map::Entry::Occupied(entry) => {
                        let Slot::UpdateGroup { sets, .. } = &mut slots[*entry.get()] else {
                            unreachable!("update_groups only indexes UpdateGroup slots");
                        };
                        sets.push((column.clone(), value.clone()));
                    }
                    std::collections::hash_map::Entry::Vacant(entry) => {
                        entry.insert(slots.len());
                        slots.push(Slot::UpdateGroup {
                            locator: locator.clone(),
                            first_index: index,
                            sets: vec![(column.clone(), value.clone())],
                        });
                    }
                }
            }
            StagedChange::Insert { columns, values } => {
                if columns.len() != values.len() {
                    return Err(StagedError {
                        change_index: Some(index),
                        change_summary: Some(change.describe()),
                        message: format!(
                            "insert names {} columns but carries {} values",
                            columns.len(),
                            values.len()
                        ),
                    });
                }
                slots.push(Slot::Ready {
                    statement: insert_statement(table, dialect, &casts, columns, values),
                    meta: StatementMeta {
                        change_index: index,
                        kind: StatementKind::Insert,
                        summary: change.describe(),
                    },
                });
            }
            StagedChange::Delete { locator } => {
                check_locator(locator, index, change)?;
                slots.push(Slot::Ready {
                    statement: delete_statement(table, identity, dialect, &casts, locator),
                    meta: StatementMeta {
                        change_index: index,
                        kind: StatementKind::Delete,
                        summary: change.describe(),
                    },
                });
            }
        }
    }
    let mut statements = Vec::with_capacity(slots.len());
    let mut metas = Vec::with_capacity(slots.len());
    for slot in slots {
        let (statement, meta) = match slot {
            Slot::UpdateGroup {
                locator,
                first_index,
                sets,
            } => (
                update_statement(table, identity, dialect, &casts, &locator, &sets),
                StatementMeta {
                    change_index: first_index,
                    kind: StatementKind::Update,
                    summary: update_summary(&locator, &sets),
                },
            ),
            Slot::Ready { statement, meta } => (statement, meta),
        };
        statements.push(statement);
        metas.push(meta);
    }
    Ok((statements, metas))
}

/// Failure summary for a (possibly multi-column) row update: names the row
/// and every column the statement sets.
fn update_summary(locator: &RowLocator, sets: &[(String, Value)]) -> String {
    let columns: Vec<&str> = sets.iter().map(|(column, _)| column.as_str()).collect();
    format!(
        "update of row {} [columns {}]",
        locator.summary(),
        columns.join(", ")
    )
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
    /// On Postgres a non-NULL placeholder carries the column's cast when
    /// one is known (`$1::integer` — see [`cast_target`]), so text-staged
    /// values (the editor stages rich Postgres types as text, FRE-24)
    /// coerce to the column type instead of failing the bind-type check.
    ///
    /// The same rendering applies in WHERE key clauses: `col = NULL` is
    /// never true, so a NULL identity value matches nothing and the
    /// row-count guard aborts the batch — the safe outcome, since a NULL
    /// key cannot address a row anyway.
    fn value_sql(&mut self, value: &Value, cast: Option<&str>) -> String {
        if value.is_null() {
            return "NULL".to_string();
        }
        self.values.push(value.clone());
        let placeholder = self.dialect.placeholder(self.values.len());
        match cast {
            // `cast` is only ever `Some` when [`cast_targets`] decided this
            // dialect needs one (never on SQLite).
            Some(cast) => self.dialect.cast_expr(&placeholder, cast),
            None => placeholder,
        }
    }
}

/// Cast targets for every castable column of `table`, keyed by column
/// name — computed ONCE per [`build_statements`] call so rendering a value
/// looks its column up in O(1) instead of re-scanning `table.columns` per
/// SET/key value (FRE-131). A column absent from the map (not castable, not
/// introspected at all, or any non-Postgres dialect — SQLite's type
/// affinity coerces on its own) binds its placeholder uncast, exactly as
/// before.
fn cast_targets(table: &TableMeta, dialect: Dialect) -> HashMap<&str, String> {
    if dialect != Dialect::Postgres {
        return HashMap::new();
    }
    table
        .columns
        .iter()
        .filter_map(|column| cast_target(column).map(|cast| (column.name.as_str(), cast)))
        .collect()
}

/// The Postgres cast target for one column's bound parameters, derived from
/// the introspected column type.
///
/// Why: sqlx binds [`Value::Text`] as a *text-typed* parameter, and
/// Postgres refuses `SET int_col = $1` for it (documented on
/// `postgres::bind_params`). The editor (FRE-24) stages every rich
/// Postgres value — timestamps, numerics, json, booleans — as text, so
/// without a cast none of them could be saved. `information_schema`
/// `data_type` strings ("integer", "timestamp without time zone", "jsonb",
/// "numeric", …) are themselves valid cast targets, so the introspected
/// type is used verbatim. Casts also apply to WHERE key values, where e.g.
/// a uuid or timestamp key arrives from the grid as text.
///
/// Enum and array columns are the exception to "use `data_type` verbatim":
/// they report `USER-DEFINED` and `ARRAY`, which name no type at all. For
/// those the introspected [`TypeDetail`] supplies the real type name as its
/// two identifier halves, each double-quoted here (`"public"."mood"`,
/// `"pg_catalog"."_text"`). Quoted rather than lowercased because both are
/// arbitrary identifiers — `CREATE TYPE "Mood"` is case-sensitive — and
/// schema-qualified because a user enum lives in its own schema while
/// built-in array types live in `pg_catalog`, so neither is guaranteed to
/// resolve through `search_path` (FRE-71).
///
/// Skipped (`None`) — the placeholder then binds as before (unknown columns
/// and non-Postgres dialects never reach here; [`cast_targets`] leaves them
/// out of the map):
/// - when the column's type name is empty;
/// - for `data_type` strings that are not usable/plain type names:
///   `ARRAY` and `USER-DEFINED` whose real name didn't resolve, or anything
///   outside `[a-z0-9 _.]` after lowercasing — `.` is allowed because the
///   materialized-view half of the introspection UNION fills `type_name`
///   from `format_type()`, which can legitimately emit `myschema.mytype`
///   (the charset gate also guarantees the interpolated cast text is inert);
/// - for type names whose **bare form implies a restrictive default
///   modifier**: `data_type` drops length modifiers, so a `character(3)`
///   column reports just "character" — and `::character` means `char(1)`,
///   which would silently TRUNCATE the value to one character on SET (and
///   make a `char(n)` key column never match its row, aborting every
///   save); `::bit` likewise means `bit(1)` and errors loudly. These are
///   exactly the bare names with restrictive defaults — `character
///   varying` and `numeric` stay castable because their bare forms are
///   unbounded/unconstrained. For the skipped types the uncast text
///   parameter is correct: Postgres's assignment/comparison coercion
///   handles text → char(n)/bit(n) with the column's true modifier.
fn cast_target(column: &ColumnMeta) -> Option<String> {
    // An enum/array column casts to its real type name, which `data_type`
    // doesn't carry (FRE-71). Both halves are arbitrary identifiers — a
    // `CREATE TYPE "Mood"` is case-sensitive and would not resolve
    // lowercased — so they are quoted rather than run through the
    // lowercase/charset gate that the `data_type` vocabulary below needs.
    if let Some(type_ref) = column.type_detail.cast_type() {
        return Some(format!(
            "{}.{}",
            quote_ident(&type_ref.schema),
            quote_ident(&type_ref.name)
        ));
    }
    let lowered = column.type_name.trim().to_ascii_lowercase();
    let plain = !lowered.is_empty()
        && lowered != "array"
        && lowered != "character"
        && lowered != "bit"
        && lowered.chars().all(|c| {
            c.is_ascii_lowercase() || c.is_ascii_digit() || c == ' ' || c == '_' || c == '.'
        });
    plain.then_some(lowered)
}

/// `UPDATE t SET a = ?, b = NULL WHERE k1 = ? AND k2 = ?`, guarded to one
/// row.
fn update_statement(
    table: &TableMeta,
    identity: &RowIdentity,
    dialect: Dialect,
    casts: &HashMap<&str, String>,
    locator: &RowLocator,
    sets: &[(String, Value)],
) -> CheckedStatement {
    debug_assert!(!sets.is_empty(), "UPDATE needs at least one SET");
    let mut params = ParamSql::new(dialect);
    let assignments: Vec<String> = sets
        .iter()
        .map(|(column, value)| {
            let cast = casts.get(column.as_str()).map(String::as_str);
            format!(
                "{} = {}",
                quote_ident(column),
                params.value_sql(value, cast)
            )
        })
        .collect();
    let sql = format!(
        "UPDATE {} SET {} WHERE {}",
        qualified_table(table),
        assignments.join(", "),
        key_clause(identity, casts, locator, &mut params),
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
    casts: &HashMap<&str, String>,
    columns: &[String],
    values: &[Value],
) -> CheckedStatement {
    let mut params = ParamSql::new(dialect);
    let sql = if columns.is_empty() {
        format!("INSERT INTO {} DEFAULT VALUES", qualified_table(table))
    } else {
        let names: Vec<String> = columns.iter().map(|c| quote_ident(c)).collect();
        let rendered: Vec<String> = columns
            .iter()
            .zip(values)
            .map(|(column, value)| {
                let cast = casts.get(column.as_str()).map(String::as_str);
                params.value_sql(value, cast)
            })
            .collect();
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
    casts: &HashMap<&str, String>,
    locator: &RowLocator,
) -> CheckedStatement {
    let mut params = ParamSql::new(dialect);
    let sql = format!(
        "DELETE FROM {} WHERE {}",
        qualified_table(table),
        key_clause(identity, casts, locator, &mut params),
    );
    CheckedStatement {
        sql,
        params: params.values,
        expected_rows: 1,
    }
}

/// `"k1" = ? AND "k2" = NULL` over the full key, pairing the identity's key
/// columns with the locator's values (arity is validated by the caller).
/// Key placeholders carry column casts too ([`cast_targets`]) — identity
/// values of rich Postgres types (uuid, timestamp, numeric keys) arrive
/// from the grid as text and must coerce in the WHERE clause as well.
fn key_clause(
    identity: &RowIdentity,
    casts: &HashMap<&str, String>,
    locator: &RowLocator,
    params: &mut ParamSql,
) -> String {
    identity
        .key_columns()
        .iter()
        .zip(&locator.identity_values)
        .map(|(column, value)| {
            let cast = casts.get(*column).map(String::as_str);
            format!(
                "{} = {}",
                quote_ident(column),
                params.value_sql(value, cast)
            )
        })
        .collect::<Vec<_>>()
        .join(" AND ")
}

/// Thin wrapper over [`super::sql::qualified`] for a table's own name.
fn qualified_table(table: &TableMeta) -> String {
    qualified(table.schema.as_deref(), &table.name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::schema::{ColumnMeta, Generated, TableKind};

    fn col(name: &str, pk: Option<u32>) -> ColumnMeta {
        ColumnMeta {
            name: name.into(),
            type_name: "TEXT".into(),
            nullable: pk.is_none(),
            primary_key_position: pk,
            default: None,
            generated: Generated::Never,
            type_detail: crate::db::TypeDetail::Plain,
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
            restriction: None,
            internal: None,
            kind_label: None,
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
        let (statements, metas) = build_statements(
            &table(),
            &identity(),
            Dialect::Sqlite,
            &[
                update(1, "a", Value::Text("x".into())),
                update(1, "b", Value::Integer(2)),
            ],
        )
        .unwrap();
        assert_eq!(statements.len(), 1);
        assert_eq!(metas.len(), 1);
        assert_eq!(
            statements[0].sql,
            "UPDATE \"t\" SET \"a\" = ?, \"b\" = ? WHERE \"id\" = ?"
        );
        assert_eq!(
            statements[0].params,
            vec![
                Value::Text("x".into()),
                Value::Integer(2),
                Value::Integer(1)
            ]
        );
        assert_eq!(statements[0].expected_rows, 1);
        assert_eq!(metas[0].change_index, 0);
        // The failure summary names the row and BOTH columns.
        assert_eq!(metas[0].summary, "update of row (1) [columns a, b]");
    }

    #[test]
    fn grouping_is_by_locator_not_adjacency_and_attributes_the_first_change() {
        let (statements, metas) = build_statements(
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
        assert_eq!(statements.len(), 2);
        assert_eq!(metas[0].change_index, 0);
        assert!(statements[0].sql.contains("\"a\" = ?, \"b\" = ?"));
        assert_eq!(metas[1].change_index, 1);
        assert_eq!(metas[1].summary, "update of row (2) [columns a]");
    }

    #[test]
    fn grouping_matches_row_keys_not_float_equality() {
        let real_update = |bits: f64, column: &str| StagedChange::Update {
            locator: RowLocator {
                identity_values: vec![Value::Real(bits)],
            },
            column: column.into(),
            value: Value::Integer(1),
        };
        // 0.0 == -0.0 by PartialEq, but they are two different stage rows
        // (bit-distinct keys) — merging them would build one UPDATE with a
        // duplicate SET column (a Postgres error) against the wrong row.
        let (statements, _) = build_statements(
            &table(),
            &identity(),
            Dialect::Sqlite,
            &[real_update(0.0, "a"), real_update(-0.0, "a")],
        )
        .unwrap();
        assert_eq!(statements.len(), 2, "0.0 and -0.0 must stay separate rows");

        // NaN != NaN by PartialEq, but it is one stage row — its column
        // edits must group into one statement, not repeat the UPDATE.
        let (statements, _) = build_statements(
            &table(),
            &identity(),
            Dialect::Sqlite,
            &[real_update(f64::NAN, "a"), real_update(f64::NAN, "b")],
        )
        .unwrap();
        assert_eq!(statements.len(), 1, "NaN groups with itself");
        assert!(statements[0].sql.contains("\"a\" = ?, \"b\" = ?"));
    }

    #[test]
    fn null_values_render_as_literal_null_never_as_parameters() {
        // UPDATE: SET NULL inline; only the non-NULL set value and the key
        // are bound (with their column casts — the fixture columns are
        // TEXT). This is what makes `SET int_col = NULL` work on Postgres,
        // where a bound Value::Null is typed as text.
        let (statements, _) = build_statements(
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
            statements[0].sql,
            "UPDATE \"app\".\"t\" SET \"a\" = NULL, \"b\" = $1::text WHERE \"id\" = $2::text"
        );
        assert_eq!(
            statements[0].params,
            vec![Value::Text("x".into()), Value::Integer(1)]
        );

        // INSERT: NULL inline in VALUES.
        let (statements, _) = build_statements(
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
            statements[0].sql,
            "INSERT INTO \"app\".\"t\" (\"id\", \"a\", \"b\") VALUES ($1::text, NULL, $2::text)"
        );
        assert_eq!(
            statements[0].params,
            vec![Value::Integer(5), Value::Text("y".into())]
        );

        // WHERE: a NULL key value renders inline too; `col = NULL` matches
        // nothing, so the row-count guard aborts instead of erroring on a
        // typed-NULL bind.
        let (statements, _) = build_statements(
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
            statements[0].sql,
            "DELETE FROM \"app\".\"t\" WHERE \"id\" = NULL"
        );
        assert!(statements[0].params.is_empty());
    }

    fn typed_pg_table() -> TableMeta {
        let typed = |name: &str, type_name: &str, pk: Option<u32>| ColumnMeta {
            name: name.into(),
            type_name: type_name.into(),
            nullable: pk.is_none(),
            primary_key_position: pk,
            default: None,
            generated: Generated::Never,
            type_detail: crate::db::TypeDetail::Plain,
        };
        TableMeta {
            schema: Some("app".into()),
            name: "typed".into(),
            kind: TableKind::Table,
            columns: vec![
                typed("id", "integer", Some(1)),
                typed("flag", "boolean", None),
                typed("at", "timestamp without time zone", None),
                typed("doc", "jsonb", None),
                typed("amount", "numeric", None),
                typed("tags", "ARRAY", None),
                typed("mood", "USER-DEFINED", None),
                typed("legacy", "", None),
                typed("code", "character", None),
                typed("mask", "bit", None),
                typed("nick", "character varying", None),
            ],
            indexes: vec![],
            foreign_keys: vec![],
            restriction: None,
            internal: None,
            kind_label: None,
        }
    }

    #[test]
    fn postgres_params_carry_column_casts_from_introspected_types() {
        let (statements, _) = build_statements(
            &typed_pg_table(),
            &identity(),
            Dialect::Postgres,
            &[
                update(1, "flag", Value::Text("true".into())),
                update(1, "at", Value::Text("2024-06-01 12:30:00".into())),
                update(1, "doc", Value::Text("{\"a\":1}".into())),
                update(1, "amount", Value::Text("12345678901234567890.5".into())),
            ],
        )
        .unwrap();
        assert_eq!(
            statements[0].sql,
            "UPDATE \"app\".\"typed\" SET \
             \"flag\" = $1::boolean, \
             \"at\" = $2::timestamp without time zone, \
             \"doc\" = $3::jsonb, \
             \"amount\" = $4::numeric \
             WHERE \"id\" = $5::integer"
        );

        // Casts apply to INSERT values and DELETE keys too.
        let (statements, _) = build_statements(
            &typed_pg_table(),
            &identity(),
            Dialect::Postgres,
            &[
                StagedChange::Insert {
                    columns: vec!["id".into(), "flag".into()],
                    values: vec![Value::Integer(2), Value::Text("false".into())],
                },
                StagedChange::Delete {
                    locator: locator(9),
                },
            ],
        )
        .unwrap();
        assert_eq!(
            statements[0].sql,
            "INSERT INTO \"app\".\"typed\" (\"id\", \"flag\") VALUES ($1::integer, $2::boolean)"
        );
        assert_eq!(
            statements[1].sql,
            "DELETE FROM \"app\".\"typed\" WHERE \"id\" = $1::integer"
        );
    }

    #[test]
    fn enum_and_array_columns_cast_to_their_qualified_type_name() {
        // `data_type` is USER-DEFINED/ARRAY for these, so the cast comes
        // from the introspected TypeDetail instead (FRE-71). Without it
        // Postgres rejects the text-bound parameter outright.
        let mut table = typed_pg_table();
        for column in &mut table.columns {
            match column.name.as_str() {
                "mood" => {
                    column.type_detail = crate::db::TypeDetail::Enum {
                        type_ref: crate::db::TypeRef {
                            schema: "app".into(),
                            name: "mood".into(),
                        },
                        variants: vec!["sad".into(), "happy".into()],
                    }
                }
                "tags" => {
                    column.type_detail = crate::db::TypeDetail::Array {
                        type_ref: crate::db::TypeRef {
                            schema: "pg_catalog".into(),
                            name: "_text".into(),
                        },
                    }
                }
                _ => {}
            }
        }
        let (statements, _) = build_statements(
            &table,
            &identity(),
            Dialect::Postgres,
            &[
                update(1, "mood", Value::Text("happy".into())),
                update(1, "tags", Value::Text("{a,b}".into())),
            ],
        )
        .unwrap();
        assert_eq!(
            statements[0].sql,
            "UPDATE \"app\".\"typed\" SET \
             \"mood\" = $1::\"app\".\"mood\", \
             \"tags\" = $2::\"pg_catalog\".\"_text\" \
             WHERE \"id\" = $3::integer"
        );
    }

    #[test]
    fn casts_are_skipped_for_unusable_or_unknown_types_and_on_sqlite() {
        // ARRAY / USER-DEFINED with no resolved detail are not cast targets
        // (see the test above for when the detail does resolve); an empty
        // type name and a column missing from the metadata have nothing to
        // cast to.
        let (statements, _) = build_statements(
            &typed_pg_table(),
            &identity(),
            Dialect::Postgres,
            &[
                update(1, "tags", Value::Text("{a,b}".into())),
                update(1, "mood", Value::Text("happy".into())),
                update(1, "legacy", Value::Text("x".into())),
                update(1, "ghost", Value::Text("y".into())),
            ],
        )
        .unwrap();
        assert_eq!(
            statements[0].sql,
            "UPDATE \"app\".\"typed\" SET \
             \"tags\" = $1, \"mood\" = $2, \"legacy\" = $3, \"ghost\" = $4 \
             WHERE \"id\" = $5::integer"
        );

        // Bare "character" and "bit" imply restrictive default modifiers
        // (`::character` = char(1) would TRUNCATE a character(3) value) —
        // no cast; assignment coercion handles the uncast text correctly.
        // "character varying" is unbounded when bare and keeps its cast.
        let (statements, _) = build_statements(
            &typed_pg_table(),
            &identity(),
            Dialect::Postgres,
            &[
                update(1, "code", Value::Text("xyz".into())),
                update(1, "mask", Value::Text("1010".into())),
                update(1, "nick", Value::Text("zed".into())),
            ],
        )
        .unwrap();
        assert_eq!(
            statements[0].sql,
            "UPDATE \"app\".\"typed\" SET \
             \"code\" = $1, \"mask\" = $2, \"nick\" = $3::character varying \
             WHERE \"id\" = $4::integer"
        );

        // SQLite never casts — its type affinity coerces on its own.
        let mut sqlite_table = typed_pg_table();
        sqlite_table.schema = None;
        let (statements, _) = build_statements(
            &sqlite_table,
            &identity(),
            Dialect::Sqlite,
            &[update(1, "flag", Value::Integer(1))],
        )
        .unwrap();
        assert_eq!(
            statements[0].sql,
            "UPDATE \"typed\" SET \"flag\" = ? WHERE \"id\" = ?"
        );
    }

    #[test]
    fn sqlserver_statements_use_at_p_placeholders_without_casts() {
        // Placeholders number @P1.. across SET and WHERE; no column casts
        // are emitted (cast_target is Postgres-only — tiberius binding rules
        // are a driver concern for the connection issue).
        let mut t = typed_pg_table();
        t.schema = Some("dbo".into());
        let (statements, _) = build_statements(
            &t,
            &identity(),
            Dialect::SqlServer,
            &[
                update(1, "flag", Value::Integer(1)),
                update(1, "amount", Value::Text("12.5".into())),
                StagedChange::Insert {
                    columns: vec!["id".into(), "flag".into()],
                    values: vec![Value::Integer(2), Value::Null],
                },
                StagedChange::Delete {
                    locator: locator(9),
                },
            ],
        )
        .unwrap();
        assert_eq!(
            statements[0].sql,
            "UPDATE \"dbo\".\"typed\" SET \"flag\" = @P1, \"amount\" = @P2 WHERE \"id\" = @P3"
        );
        assert_eq!(
            statements[1].sql,
            "INSERT INTO \"dbo\".\"typed\" (\"id\", \"flag\") VALUES (@P1, NULL)"
        );
        assert_eq!(
            statements[2].sql,
            "DELETE FROM \"dbo\".\"typed\" WHERE \"id\" = @P1"
        );
    }

    #[test]
    fn plan_preserves_first_occurrence_order_across_kinds() {
        let (statements, metas) = build_statements(
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
        assert_eq!(statements.len(), metas.len());
        assert_eq!(
            metas.iter().map(|m| m.kind).collect::<Vec<_>>(),
            [
                StatementKind::Update,
                StatementKind::Insert,
                StatementKind::Delete
            ]
        );
        assert_eq!(
            metas.iter().map(|m| m.change_index).collect::<Vec<_>>(),
            [0, 1, 2]
        );
    }

    #[test]
    fn empty_column_insert_uses_default_values() {
        let (statements, _) = build_statements(
            &table(),
            &identity(),
            Dialect::Sqlite,
            &[StagedChange::Insert {
                columns: vec![],
                values: vec![],
            }],
        )
        .unwrap();
        assert_eq!(statements[0].sql, "INSERT INTO \"t\" DEFAULT VALUES");
    }

    #[test]
    fn composite_key_delete_targets_the_full_key() {
        let composite = RowIdentity::PrimaryKey {
            columns: vec!["k1".into(), "k2".into()],
        };
        let (statements, _) = build_statements(
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
            statements[0].sql,
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
        assert_eq!(err.change_summary, Some("delete of row ()".into()));
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
    fn staged_error_display_is_one_based_and_carries_the_summary() {
        let err = StagedError {
            change_index: Some(2),
            change_summary: Some("update of row (1) [columns a]".into()),
            message: "boom".into(),
        };
        assert_eq!(
            err.to_string(),
            "change 3 (update of row (1) [columns a]) failed: boom"
        );
        let err = StagedError {
            change_index: Some(2),
            change_summary: None,
            message: "boom".into(),
        };
        assert_eq!(err.to_string(), "change 3 failed: boom");
        let err = StagedError {
            change_index: None,
            change_summary: None,
            message: "boom".into(),
        };
        assert_eq!(err.to_string(), "boom");
    }
}
