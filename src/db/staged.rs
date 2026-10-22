//! Staged edits: pending cell updates, row inserts, and row
//! deletes, applied together in ONE transaction.
//!
//! The UI accumulates [`StagedChange`]s per table view (`ui::stage`) and
//! hands the normalized list to [`apply_staged`], which turns it into
//! guarded statements — every UPDATE/DELETE must affect exactly one row —
//! and runs them all inside a single transaction via
//! [`DbPool::execute_all_checked`]. Any failure (SQL error or row-count
//! mismatch) rolls the whole batch back and reports *which* change failed.
//!
//! Row addressing builds on the row-identity model
//! ([`RowIdentity`](super::rowkey::RowIdentity)); the SQL builders here are
//! value-aware — they must know the values (not just the columns) so NULLs
//! can be rendered inline (see [`ParamSql::value_sql`]). They are the only
//! UPDATE/DELETE builders in the crate: `rowkey` reports how a row is
//! addressed, this module writes it.
//!
//! On Postgres every bound parameter is cast to its column's introspected
//! type (`SET "col" = $1::integer`) so text-staged values coerce — see
//! [`cast_target`] for why and when the cast is skipped.

use std::borrow::Cow;
use std::collections::HashMap;
use std::fmt;
use std::fmt::Write as _;

use super::caps::{self, TableAccess};
use super::error::DbError;
use super::page::KeyEquality;
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
///   exactly one row aborts and rolls back the whole batch (the no-PK
///   safety net, batch-wide).
/// - NULL values are rendered inline as literal `NULL` (never as bound
///   parameters) in SET lists and INSERT values — see [`ParamSql::value_sql`].
///
/// `access` — the connection's capabilities resolved for `table`,
/// including the user's own write protection — is checked first:
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
    // struct; the metadata is only consulted on failure.
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
/// combined struct just to reach the executor.
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
    let check_locator =
        |locator: &RowLocator, index: usize, change: &StagedChange| match locator_mismatch(
            identity, locator,
        ) {
            None => Ok(()),
            Some(message) => Err(StagedError {
                change_index: Some(index),
                change_summary: Some(change.describe()),
                message,
            }),
        };
    let casts = cast_targets(table, dialect);
    let mut slots: Vec<Slot> = Vec::new();
    // Row key → index into `slots` of the row's UpdateGroup. The Vec keeps
    // first-occurrence order; the map makes the group lookup O(1) instead
    // of a linear scan over every prior slot, which made large staged
    // batches quadratic in string compares. Grouping is by the
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
                    statement: delete_statement(table, identity, dialect, locator),
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
///
/// Visible to the rest of `db/` so the file import renders its
/// multi-row INSERTs through the identical logic — an imported value and a
/// hand-edited one must reach the same column the same way, NULL literals and
/// Postgres casts included.
pub(super) struct ParamSql {
    dialect: Dialect,
    values: Vec<Value>,
}

impl ParamSql {
    pub(super) fn new(dialect: Dialect) -> Self {
        ParamSql {
            dialect,
            values: Vec::new(),
        }
    }

    /// The parameters accumulated so far, in bind order.
    pub(super) fn into_values(self) -> Vec<Value> {
        self.values
    }

    /// A bound placeholder for `value` — except NULL, which is rendered as
    /// the literal `NULL` (safe: not user text). Binding NULLs would break
    /// on Postgres: the drivers bind `Value::Null` as a NULL *of type text*
    /// (see `postgres::bind_params`), and Postgres rejects a text NULL for
    /// e.g. `SET int_col = $1`. A literal `NULL` has no type until the
    /// column gives it one, so `SET int_col = NULL` and
    /// `INSERT … VALUES (NULL, …)` just work on both backends.
    ///
    /// A non-NULL placeholder carries the column's cast when [`column_cast`]
    /// gives one: on Postgres every plain type (`$1::integer` — see
    /// [`cast_target`]), so text-staged values (the editor stages rich
    /// Postgres types as text) coerce to the column type instead of failing
    /// the bind-type check; on SQL Server only a `real`
    /// (`CAST(@P1 AS real)`), bound typed.
    ///
    /// **On Postgres, under a cast every value binds as text**
    /// ([`cast_text`]), whatever [`Value`] it is, and the cast parses it. sqlx caches a prepared
    /// statement by its SQL text alone, with the parameter types of its first
    /// execution, and the cast — part of the text — is decided by the
    /// *column*, while a typed bind would be decided by the *value*. The cell
    /// editor stages `42` in a `double precision` cell as an integer and `1.5`
    /// as a float; a foreign-key jump into one column binds whatever its
    /// source column decoded to. Bound typed, the second of two such
    /// statements on a session ran against the first one's types: an
    /// integer refused by a `numeric` text bind ("insufficient data left in
    /// message"), or — silently — a float's bits read as an `int8`, landing a
    /// jump on an empty page and writing a wrong number. A text bind under a
    /// cast is one type for every value, so one statement is one bind shape.
    /// Gated on the dialect, not on there being a cast: it rests on sqlx's
    /// Postgres cache and Postgres's text input functions, and a cast another
    /// dialect gains ([`column_cast`]) keeps its typed bind.
    ///
    /// The same rendering applies in WHERE key clauses: `col = NULL` is
    /// never true, so a NULL identity value matches nothing and the
    /// row-count guard aborts the batch — the safe outcome, since a NULL
    /// key cannot address a row anyway.
    pub(super) fn value_sql(&mut self, value: &Value, cast: Option<&str>) -> String {
        if value.is_null() {
            return "NULL".to_string();
        }
        match cast {
            // `cast` comes from [`column_cast`], the one place that decides
            // whether a column's binds carry one.
            Some(cast) => {
                self.values.push(match self.dialect {
                    Dialect::Postgres => Value::Text(cast_text(value)),
                    Dialect::Sqlite | Dialect::SqlServer => value.clone(),
                });
                let placeholder = self.dialect.placeholder(self.values.len());
                self.dialect.cast_expr(&placeholder, cast)
            }
            None => {
                self.values.push(value.clone());
                self.dialect.placeholder(self.values.len())
            }
        }
    }
}

/// The text a non-NULL value binds as under a Postgres cast, in the input
/// syntax the cast parses back to the same value: integers in decimal, a
/// float as its shortest round-trip spelling ([`float_spelling`]), a blob in
/// `bytea`'s `\x` hex form, text as itself.
fn cast_text(value: &Value) -> String {
    match value {
        Value::Text(text) => text.clone(),
        Value::Integer(n) => n.to_string(),
        Value::Real(wide) => float_spelling(*wide),
        Value::Blob(bytes) => {
            let mut hex = String::with_capacity(2 + bytes.len() * 2);
            hex.push_str("\\x");
            for byte in bytes {
                let _ = write!(hex, "{byte:02x}");
            }
            hex
        }
        // Rendered as the literal `NULL` before a bind is ever made.
        Value::Null => String::new(),
    }
}

/// The shortest decimal that reads back as exactly `wide`, in a form every
/// PostgreSQL-family engine's float and numeric input accepts: Rust's
/// exponent notation (`1e20`, `1e-1`), with the non-finite values spelled
/// out, since `inf` is Rust's spelling and not every engine's.
fn float_spelling(wide: f64) -> String {
    if wide.is_nan() {
        "NaN".to_string()
    } else if wide.is_infinite() {
        if wide > 0.0 { "Infinity" } else { "-Infinity" }.to_string()
    } else {
        format!("{wide:e}")
    }
}

/// Cast targets for every castable column of `table`, keyed by column
/// name — computed ONCE per [`build_statements`] call so rendering a value
/// looks its column up in O(1) instead of re-scanning `table.columns` per
/// SET/key value. A column absent from the map (not castable, not
/// introspected at all, SQLite, and every SQL Server column but a `real`)
/// binds its placeholder uncast, exactly as before. Each entry is
/// [`column_cast`]'s answer.
pub(super) fn cast_targets(table: &TableMeta, dialect: Dialect) -> HashMap<&str, String> {
    table
        .columns
        .iter()
        .filter_map(|column| column_cast(column, dialect).map(|cast| (column.name.as_str(), cast)))
        .collect()
}

/// The cast a value bound against `column` carries on `dialect`, or `None`
/// for an uncast placeholder — the one decision every bound position takes
/// its cast from: SETs and inserts (through [`cast_targets`]), the import,
/// and key predicates ([`key_equality_sql`]), so a column cannot be cast in
/// one of them and not in another. Postgres casts every plain type
/// ([`cast_target`]); SQLite's type affinity coerces on its own; SQL Server
/// converts a parameter implicitly, except into a `real`.
///
/// SQL Server's one cast is `real`. A `real` cell reads as the shortest
/// decimal that is its `f32` ([`super::value::real_from_f32`]), and tiberius
/// binds that as a `float`: compared uncast, the column widens to `float`
/// and `0.1` (the float64) never equals the widened `0.1f32`, so a `real`
/// key re-addresses no row and a foreign key into one jumps to nothing.
/// `CAST(@P1 AS real)` rounds the parameter to the column's own `f32` first.
pub(super) fn column_cast(column: &ColumnMeta, dialect: Dialect) -> Option<String> {
    match dialect {
        Dialect::Postgres => cast_target(column),
        Dialect::SqlServer => column
            .type_name
            .trim()
            .eq_ignore_ascii_case("real")
            .then(|| "real".to_string()),
        Dialect::Sqlite => None,
    }
}

/// The Postgres cast target for one column's bound parameters, derived from
/// the introspected column type.
///
/// Why: sqlx binds [`Value::Text`] as a *text-typed* parameter, and
/// Postgres refuses `SET int_col = $1` for it (documented on
/// `postgres::bind_params`). The editor stages every rich
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
/// resolve through `search_path`.
///
/// Skipped (`None`) — the placeholder then binds as before (unknown columns
/// and other dialects never reach here; [`column_cast`] answers for them):
/// - when the column's type name is empty;
/// - for `data_type` strings that are not usable/plain type names:
///   `ARRAY` and `USER-DEFINED` whose real name didn't resolve, or anything
///   outside `[a-z0-9 _.]` after lowercasing and setting aside a trailing
///   `[]` (an array spelled by its element type) — `.` is allowed because the
///   materialized-view half of the introspection UNION fills `type_name`
///   from `format_type()`, which can legitimately emit `myschema.mytype`
///   (the charset gate also guarantees the interpolated cast text is inert).
///
/// Two bare names are **rewritten**, because each implies a restrictive
/// default modifier: `data_type` drops length modifiers, so a `bit(4)` or
/// `character(3)` column reports just "bit" or "character", and `::bit` means
/// `bit(1)`, `::character` means `char(1)` — a cast that would silently
/// TRUNCATE the value on SET. Each casts to the unbounded member of its family
/// instead, which the server then assigns into the column under its true
/// modifier. `character varying` and `numeric` pass through verbatim because
/// their bare forms are already unbounded.
///
/// - `bit` → `bit varying`. Leaving it uncast is not an option: text has no
///   assignment coercion to `bit`, so `bit(n)` columns were unwritable
///   outright (*"column is of type bit but expression is of type text"*).
/// - `character` → `bpchar`, the blank-padded type with no length. This one
///   used to be left uncast, which assigns correctly but does not *compare*
///   correctly: the grid reads a `character(3)` holding `ab` as `"ab "`, and
///   `char = text` resolves to a text comparison that strips the column's
///   padding and keeps the parameter's, so a key shorter than its width
///   never matched its row and every save of it was refused. `bpchar`
///   compares blank-insensitively, as the column does. Measured on
///   PostgreSQL 17 and CockroachDB, SET through `$1::bpchar` and through an
///   uncast text parameter store the same bytes and refuse the same
///   over-long value.
fn cast_target(column: &ColumnMeta) -> Option<String> {
    // An enum/array column casts to its real type name, which `data_type`
    // doesn't carry. Both halves are arbitrary identifiers — a
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
    // An array spelled as its element type with `[]` — what RisingWave's
    // `data_type` says (`character varying[]`, with a `udt_name` of `list`
    // that names no `pg_type` row, so no [`TypeDetail`] resolves) and what
    // `format_type()` prints for a materialized view's array column. The
    // suffix is kept and the element type goes through the same gate and
    // rewrites as a scalar. Uncast, RisingWave refused the key predicate
    // outright (`"k" = $1` binds text against `varchar[]`), so a long cell
    // keyed by an array never loaded.
    let mut element = lowered.as_str();
    let mut dims = 0;
    while let Some(inner) = element.strip_suffix("[]") {
        element = inner.trim_end();
        dims += 1;
    }
    // Bare `bit` and `character` are `bit(n)` and `character(n)`, whose
    // modifier `data_type` dropped. Cast to the unbounded member of the
    // family and let the column's own modifier apply on assignment.
    let element = match element {
        "bit" => "bit varying",
        "character" => "bpchar",
        other => other,
    };
    let plain = !element.is_empty()
        && element != "array"
        && element.chars().all(|c| {
            c.is_ascii_lowercase() || c.is_ascii_digit() || c == ' ' || c == '_' || c == '.'
        });
    plain.then(|| format!("{element}{}", "[]".repeat(dims)))
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
        key_clause(table, identity, locator, &mut params),
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
    locator: &RowLocator,
) -> CheckedStatement {
    let mut params = ParamSql::new(dialect);
    let sql = format!(
        "DELETE FROM {} WHERE {}",
        qualified_table(table),
        key_clause(table, identity, locator, &mut params),
    );
    CheckedStatement {
        sql,
        params: params.values,
        expected_rows: 1,
    }
}

/// Why `locator` cannot address a row of `identity`, or `None` when it can.
///
/// It needs exactly one value per key column. [`key_clause`] pairs the two up
/// by position, so a locator built against an older identity — the key gained
/// or lost a column after a re-introspection — would otherwise pin fewer
/// columns than the key has and match any row sharing that prefix: a write
/// would be refused by its row-count guard, but a read has no guard and would
/// return whichever row came first as if it were the one asked for. An empty
/// key is refused for the same reason, since it pins nothing at all.
pub(super) fn locator_mismatch(identity: &RowIdentity, locator: &RowLocator) -> Option<String> {
    let key_len = identity.key_columns().len();
    let carried = locator.identity_values.len();
    let plural =
        |n: usize, one: &str, many: &str| format!("{n} {}", if n == 1 { one } else { many });
    (carried != key_len || key_len == 0).then(|| {
        format!(
            "the table's key has changed since this page was loaded — reload the table and \
             try again (the row was read with {}, but the key now has {})",
            plural(carried, "key value", "key values"),
            plural(key_len, "column", "columns"),
        )
    })
}

/// ` WHERE "k1" = ? AND "k2" = ?` and its parameters: the predicate that
/// re-addresses one grid row from its [`RowLocator`], for reading as well as
/// writing.
///
/// [`DbPool::fetch_cell`](super::DbPool::fetch_cell) builds its WHERE here
/// rather than comparing the key as text the way the filter bar does, so the
/// expand popup, the truncated-cell editor and the clipboard find a row by
/// the very predicate Save then writes it through. A foreign-key jump
/// compares term for term the same way ([`equalities_where`]). Comparing as
/// text was a second answer to the same question, and a wrong one wherever the server spells a
/// value differently from hubro: a float key (`float8::text` of 1e20 is
/// `1e+20`, SQL Server's `CAST(float AS nvarchar)` keeps six significant
/// digits), a blob key (bound as the marker `<blob N>`), and on SQL Server
/// every date/time type and `money`. Each of those matched no row, so the
/// full value loaded as NULL while the same row saved fine.
pub(super) fn row_where(
    table: &TableMeta,
    identity: &RowIdentity,
    dialect: Dialect,
    locator: &RowLocator,
) -> Result<(String, Vec<Value>), String> {
    if let Some(message) = locator_mismatch(identity, locator) {
        return Err(message);
    }
    let mut params = ParamSql::new(dialect);
    let clause = key_clause(table, identity, locator, &mut params);
    Ok((format!(" WHERE {clause}"), params.into_values()))
}

/// `"k1" = ? AND "k2" = NULL` over the full key, pairing the identity's key
/// columns with the locator's values (arity is validated by the caller, with
/// [`locator_mismatch`]). Each value binds by its column's own type through
/// [`key_equality_sql`], so it compares by the column's own equality rather
/// than by a text spelling of the column.
fn key_clause(
    table: &TableMeta,
    identity: &RowIdentity,
    locator: &RowLocator,
    params: &mut ParamSql,
) -> String {
    identity
        .key_columns()
        .iter()
        .zip(&locator.identity_values)
        .map(|(column, value)| {
            let meta = table.columns.iter().find(|c| c.name == *column);
            key_equality_sql(column, meta, value, params)
        })
        .collect::<Vec<_>>()
        .join(" AND ")
}

/// ` WHERE "c1" = ? AND "c2" = ?` and its parameters for a foreign-key jump's
/// [`KeyEquality`] terms — an empty list is no clause at all.
///
/// The terms are the source row's typed cells, so they are compared the way
/// [`row_where`] compares a row's own key, term for term through the same
/// [`key_equality_sql`], and never as text. The text comparison this
/// replaced landed a jump on an empty page wherever hubro spells a value
/// differently from the server's own cast to text: a float (`1e+20`), a blob
/// (bound as the marker `<blob N>`), a `timestamptz` (`+00:00` against the
/// server's `+00`), a `character(n)` key (the cast to text strips the padding
/// the grid reads), CockroachDB's `numeric`, and on SQL Server every
/// date/time type and `money`.
pub(super) fn equalities_where(terms: &[KeyEquality], dialect: Dialect) -> (String, Vec<Value>) {
    if terms.is_empty() {
        return (String::new(), Vec::new());
    }
    let mut params = ParamSql::new(dialect);
    let clause = terms
        .iter()
        .map(|term| key_equality_sql(&term.column, term.target.as_ref(), &term.value, &mut params))
        .collect::<Vec<_>>()
        .join(" AND ");
    (format!(" WHERE {clause}"), params.into_values())
}

/// `"column" = $n::cast` for one key value against the column `meta`
/// describes — the one rendering every key predicate shares. The column
/// decides both the cast ([`column_cast`]) and the spelling
/// ([`key_spelling`]); a column hubro has no metadata for binds the value
/// as it is.
fn key_equality_sql(
    column: &str,
    meta: Option<&ColumnMeta>,
    value: &Value,
    params: &mut ParamSql,
) -> String {
    let dialect = params.dialect;
    let cast = meta.and_then(|meta| column_cast(meta, dialect));
    let value = match meta {
        Some(meta) => key_spelling(meta, dialect, value),
        None => Cow::Borrowed(value),
    };
    format!(
        "{} = {}",
        quote_ident(column),
        params.value_sql(&value, cast.as_deref())
    )
}

/// The value a key position binds against the column `meta` describes:
/// `value` itself, except where the server would read hubro's spelling of
/// it as some other value.
///
/// - **A SQL Server `datetime` or `smalldatetime`** binds in the ISO 8601
///   form with a `T` (`2024-02-29T13:45:01.123`) instead of the grid's
///   `2024-02-29 13:45:01.123`. SQL Server reads the space-separated form by
///   the session's `DATEFORMAT` for these two types (not for `datetime2`,
///   `date` or `datetimeoffset`): measured on SQL Server 2022 under
///   `SET LANGUAGE British`, `k = N'2024-01-02 13:45:01.123'` matches the
///   row stored for 1 February — or none, and `2024-02-29` is out of range
///   — while the `T` form matches the right row under every language.
///
/// Key positions only: a SET writes what the user typed.
fn key_spelling<'a>(meta: &ColumnMeta, dialect: Dialect, value: &'a Value) -> Cow<'a, Value> {
    let type_name = meta.type_name.trim();
    match (dialect, value) {
        (Dialect::SqlServer, Value::Text(text))
            if type_name.eq_ignore_ascii_case("datetime")
                || type_name.eq_ignore_ascii_case("smalldatetime") =>
        {
            match iso_datetime(text) {
                Some(iso) => Cow::Owned(Value::Text(iso)),
                None => Cow::Borrowed(value),
            }
        }
        _ => Cow::Borrowed(value),
    }
}

/// `YYYY-MM-DD hh:mm:ss[.fff]` respelled `YYYY-MM-DDThh:mm:ss[.fff]`, or
/// `None` for anything else — a staged value the user typed in some other
/// form is left for the server to read as it would.
fn iso_datetime(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let digits = |range: std::ops::Range<usize>| {
        bytes
            .get(range)
            .is_some_and(|part| part.iter().all(u8::is_ascii_digit))
    };
    let shaped = bytes.len() >= 19
        && digits(0..4)
        && bytes[4] == b'-'
        && digits(5..7)
        && bytes[7] == b'-'
        && digits(8..10)
        && bytes[10] == b' '
        && digits(11..13)
        && bytes[13] == b':'
        && digits(14..16)
        && bytes[16] == b':'
        && digits(17..19)
        && (bytes.len() == 19
            || (bytes[19] == b'.' && digits(20..bytes.len()) && bytes.len() > 20));
    shaped.then(|| format!("{}T{}", &text[..10], &text[11..]))
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
            vec![Value::Text("x".into()), Value::Text("1".into())]
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
            vec![Value::Text("5".into()), Value::Text("y".into())]
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

    #[test]
    fn bit_columns_cast_to_the_unbounded_member_of_their_family() {
        // `data_type` reports bare "bit" for a `bit(n)` column, dropping the
        // modifier. `::bit` would therefore mean `bit(1)`, but skipping the
        // cast is not the answer either: unlike `character`, a bit-string has
        // no assignment coercion from text, so an uncast parameter is refused
        // outright ("column is of type bit but expression is of type text")
        // and the column is unwritable. `bit varying` is unbounded,
        // so it accepts the value and the column's own modifier applies.
        let mut table = typed_pg_table();
        table.columns.push(ColumnMeta {
            name: "flags".into(),
            type_name: "bit varying".into(),
            nullable: true,
            primary_key_position: None,
            default: None,
            generated: Generated::Never,
            type_detail: crate::db::TypeDetail::Plain,
        });
        let (statements, _) = build_statements(
            &table,
            &identity(),
            Dialect::Postgres,
            &[
                update(1, "mask", Value::Text("1010".into())),
                update(1, "flags", Value::Text("1101".into())),
            ],
        )
        .unwrap();
        assert_eq!(
            statements[0].sql,
            "UPDATE \"app\".\"typed\" SET \
             \"mask\" = $1::bit varying, \"flags\" = $2::bit varying \
             WHERE \"id\" = $3::integer",
            "a bit column must carry a cast — without one the server refuses \
             the text parameter and the column cannot be written at all"
        );

        // A `bit(n)` key column has the same problem in the WHERE clause: an
        // uncast key never matches its row, which aborts the whole save.
        let mut keyed = typed_pg_table();
        for column in &mut keyed.columns {
            column.primary_key_position = match column.name.as_str() {
                "mask" => Some(1),
                _ => None,
            };
        }
        let casts = cast_targets(&keyed, Dialect::Postgres);
        assert_eq!(casts.get("mask").map(String::as_str), Some("bit varying"));

        // SQLite casts nothing, and SQL Server nothing but a `real`.
        assert!(cast_targets(&keyed, Dialect::Sqlite).is_empty());
        assert!(cast_targets(&keyed, Dialect::SqlServer).is_empty());
    }

    #[test]
    fn a_sql_server_real_key_is_compared_as_a_real() {
        // A `real` cell reads as its shortest decimal (`0.1`), which binds
        // as a float; uncast, SQL Server widens the column instead and the
        // two never meet.
        let mut table = typed_pg_table();
        table.columns = ["real", "REAL", "float", "decimal"]
            .iter()
            .enumerate()
            .map(|(n, type_name)| ColumnMeta {
                name: format!("c{n}"),
                type_name: (*type_name).into(),
                ..table.columns[0].clone()
            })
            .collect();
        let casts = cast_targets(&table, Dialect::SqlServer);
        assert_eq!(
            casts,
            HashMap::from([("c0", "real".to_string()), ("c1", "real".to_string())])
        );
        let identity = RowIdentity::PrimaryKey {
            columns: vec!["c0".into(), "c2".into()],
        };
        let locator = RowLocator {
            identity_values: vec![Value::Real(0.1), Value::Real(0.1)],
        };
        let (clause, params) = row_where(&table, &identity, Dialect::SqlServer, &locator).unwrap();
        assert_eq!(clause, " WHERE \"c0\" = CAST(@P1 AS real) AND \"c2\" = @P2");
        assert_eq!(params, vec![Value::Real(0.1), Value::Real(0.1)]);

        // A foreign-key jump into the column binds the same way…
        let jump = [KeyEquality {
            column: "c0".into(),
            value: Value::Real(0.1),
            target: Some(table.columns[0].clone()),
        }];
        assert_eq!(
            equalities_where(&jump, Dialect::SqlServer),
            (
                " WHERE \"c0\" = CAST(@P1 AS real)".to_string(),
                vec![Value::Real(0.1)]
            )
        );
        // …and so does a SET, all typed: the text bind is Postgres's rule.
        let (statements, _) = build_statements(
            &table,
            &identity,
            Dialect::SqlServer,
            &[StagedChange::Update {
                locator,
                column: "c1".into(),
                value: Value::Real(0.25),
            }],
        )
        .unwrap();
        assert!(
            statements[0].sql.contains("SET \"c1\" = CAST(@P1 AS real)"),
            "{}",
            statements[0].sql
        );
        assert_eq!(
            statements[0].params,
            [Value::Real(0.25), Value::Real(0.1), Value::Real(0.1)]
        );
    }

    #[test]
    fn a_row_is_re_addressed_by_its_typed_key_not_by_its_text() {
        // The read predicate is the write predicate: each key value binds as
        // what it is, so a float or a blob compares by the column's own
        // equality instead of by a text spelling the server does not share.
        let composite = RowIdentity::PrimaryKey {
            columns: vec!["id".into(), "a".into()],
        };
        let key = RowLocator {
            identity_values: vec![Value::Real(1e20), Value::Blob(vec![0, 255])],
        };
        let (clause, params) = row_where(&table(), &composite, Dialect::SqlServer, &key).unwrap();
        assert_eq!(clause, " WHERE \"id\" = @P1 AND \"a\" = @P2");
        assert_eq!(params, key.identity_values);

        // Postgres keys carry the column's cast, `character` widened to the
        // blank-insensitive `bpchar` so a padded key still finds its row.
        let mut keyed = typed_pg_table();
        for column in &mut keyed.columns {
            column.primary_key_position = (column.name == "code").then_some(1);
        }
        let by_code = RowIdentity::PrimaryKey {
            columns: vec!["code".into()],
        };
        let padded = RowLocator {
            identity_values: vec![Value::Text("ab ".into())],
        };
        let (clause, params) = row_where(&keyed, &by_code, Dialect::Postgres, &padded).unwrap();
        assert_eq!(clause, " WHERE \"code\" = $1::bpchar");
        assert_eq!(params, padded.identity_values);

        // It is the clause the staged DELETE writes through, character for
        // character — the two cannot drift apart.
        let (statements, _) = build_statements(
            &keyed,
            &by_code,
            Dialect::Postgres,
            &[StagedChange::Delete {
                locator: padded.clone(),
            }],
        )
        .unwrap();
        assert_eq!(
            statements[0].sql,
            format!("DELETE FROM \"app\".\"typed\"{clause}")
        );
        assert_eq!(statements[0].params, params);
    }

    #[test]
    fn a_locator_that_does_not_fit_the_key_addresses_nothing() {
        // A locator built against an older identity must not pair up by
        // position and pin a prefix of the key: that WHERE matches every row
        // sharing the prefix, and a read has no row-count guard to catch it.
        let composite = RowIdentity::PrimaryKey {
            columns: vec!["id".into(), "a".into()],
        };
        let short = locator(1);
        let message = row_where(&table(), &composite, Dialect::Sqlite, &short).unwrap_err();
        assert!(
            message.starts_with("the table's key has changed since this page was loaded")
                && message.contains("read with 1 key value, but the key now has 2 columns"),
            "{message}"
        );
        let long = RowLocator {
            identity_values: vec![Value::Integer(1), Value::Integer(2)],
        };
        assert!(row_where(&table(), &identity(), Dialect::Sqlite, &long).is_err());
        // An empty key pins nothing at all.
        let keyless = RowIdentity::PrimaryKey { columns: vec![] };
        let empty = RowLocator {
            identity_values: vec![],
        };
        assert!(locator_mismatch(&keyless, &empty).is_some());
        // A locator that fits is accepted.
        assert_eq!(locator_mismatch(&identity(), &locator(1)), None);
        assert!(row_where(&table(), &identity(), Dialect::Sqlite, &locator(1)).is_ok());
    }

    #[test]
    fn under_a_cast_every_value_binds_as_text_so_one_statement_has_one_bind_shape() {
        // sqlx caches a prepared statement by its text, with the first
        // execution's parameter types. The cast is part of the text and is
        // decided by the column; a typed bind would be decided by the value.
        // So `42` and `1.5` staged into one `double precision` cell, or a
        // jump into one column from an `int` and from a `numeric` source,
        // must bind alike — measured on PostgreSQL 17, the second of two
        // differently typed binds failed, or read a float's bits as an int8.
        let typed = TableMeta {
            columns: vec![
                ColumnMeta {
                    type_name: "integer".into(),
                    ..col("id", Some(1))
                },
                ColumnMeta {
                    type_name: "double precision".into(),
                    ..col("f", None)
                },
                ColumnMeta {
                    type_name: "bytea".into(),
                    ..col("b", None)
                },
            ],
            ..pg_table()
        };
        let set = |id: i64, f: Value| StagedChange::Update {
            locator: locator(id),
            column: "f".into(),
            value: f,
        };
        let (statements, _) = build_statements(
            &typed,
            &identity(),
            Dialect::Postgres,
            &[set(1, Value::Integer(42)), set(2, Value::Real(1.5))],
        )
        .unwrap();
        assert_eq!(statements[0].sql, statements[1].sql);
        let shape = |params: &[Value]| {
            params
                .iter()
                .map(|p| matches!(p, Value::Text(_)))
                .collect::<Vec<_>>()
        };
        assert_eq!(shape(&statements[0].params), [true, true]);
        assert_eq!(shape(&statements[1].params), [true, true]);
        assert_eq!(
            statements[1].params,
            [Value::Text("1.5e0".into()), Value::Text("2".into())]
        );
        // A blob binds in `bytea`'s hex input form.
        let (statements, _) = build_statements(
            &typed,
            &identity(),
            Dialect::Postgres,
            &[StagedChange::Update {
                locator: locator(1),
                column: "b".into(),
                value: Value::Blob(vec![0, 255, 16]),
            }],
        )
        .unwrap();
        assert_eq!(statements[0].params[0], Value::Text("\\x00ff10".into()));
        // A column with no cast (here every SQLite column) binds typed: there
        // is no cast to parse text, and nothing for the cache to split on.
        let (statements, _) = build_statements(
            &typed,
            &identity(),
            Dialect::Sqlite,
            &[set(2, Value::Real(1.5))],
        )
        .unwrap();
        assert_eq!(statements[0].params, [Value::Real(1.5), Value::Integer(2)]);
        // The text bind is Postgres's rule, not a cast's: a cast on another
        // dialect keeps the value typed.
        for dialect in [Dialect::SqlServer, Dialect::Sqlite] {
            let mut params = ParamSql::new(dialect);
            params.value_sql(&Value::Real(1.5), Some("real"));
            assert_eq!(params.into_values(), [Value::Real(1.5)], "{dialect:?}");
        }
        let mut params = ParamSql::new(Dialect::Postgres);
        params.value_sql(&Value::Real(1.5), Some("real"));
        assert_eq!(params.into_values(), [Value::Text("1.5e0".into())]);
    }

    #[test]
    fn a_sql_server_datetime_key_binds_in_the_form_no_dateformat_misreads() {
        // SQL Server reads `2024-01-02 13:45:01.123` by the session's
        // DATEFORMAT for `datetime` and `smalldatetime`: under `SET LANGUAGE
        // British` it is 1 February. The `T` form is read the same under
        // every language.
        let typed = |type_name: &str| ColumnMeta {
            type_name: type_name.into(),
            ..col("k", Some(1))
        };
        let table = |type_name: &str| TableMeta {
            columns: vec![typed(type_name), col("a", None)],
            ..table()
        };
        let by_k = RowIdentity::PrimaryKey {
            columns: vec!["k".into()],
        };
        let bound = |type_name: &str, dialect: Dialect, value: &str| {
            let key = RowLocator {
                identity_values: vec![Value::Text(value.into())],
            };
            row_where(&table(type_name), &by_k, dialect, &key)
                .unwrap()
                .1[0]
                .clone()
        };
        let text = |t: &str| Value::Text(t.into());
        for type_name in ["datetime", "smalldatetime", "DATETIME"] {
            assert_eq!(
                bound(type_name, Dialect::SqlServer, "2024-01-02 13:45:01.123"),
                text("2024-01-02T13:45:01.123"),
                "{type_name}"
            );
            assert_eq!(
                bound(type_name, Dialect::SqlServer, "2024-01-02 00:00:00"),
                text("2024-01-02T00:00:00")
            );
            // Anything not in the grid's spelling is left as it is.
            for other in [
                "2024-01-02",
                "02/01/2024 13:45",
                "2024-01-02 13:45:01.",
                "x",
            ] {
                assert_eq!(bound(type_name, Dialect::SqlServer, other), text(other));
            }
        }
        // `datetime2`, `datetimeoffset` and `date` read the space form the
        // same under every language, and text keys are text.
        for type_name in ["datetime2", "datetimeoffset", "nvarchar"] {
            assert_eq!(
                bound(type_name, Dialect::SqlServer, "2024-01-02 13:45:01.123"),
                text("2024-01-02 13:45:01.123")
            );
        }
        // Only SQL Server has the problem.
        assert_eq!(
            bound("datetime", Dialect::Sqlite, "2024-01-02 13:45:01.123"),
            text("2024-01-02 13:45:01.123")
        );
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
        // from the introspected TypeDetail instead. Without it
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
    fn an_array_spelled_by_its_element_type_casts_to_that_array() {
        // RisingWave's `data_type` for a `varchar[]` column, and
        // `format_type()` for a materialized view's array column, name the
        // element type with `[]`. The element goes through the scalar gate
        // and rewrites; the suffix stays. Uncast, RisingWave refused the key
        // predicate outright.
        let cast = |type_name: &str| {
            column_cast(
                &ColumnMeta {
                    type_name: type_name.into(),
                    ..col("k", Some(1))
                },
                Dialect::Postgres,
            )
        };
        assert_eq!(
            cast("character varying[]").as_deref(),
            Some("character varying[]")
        );
        assert_eq!(cast("integer[][]").as_deref(), Some("integer[][]"));
        assert_eq!(cast("character[]").as_deref(), Some("bpchar[]"));
        assert_eq!(cast("bit []").as_deref(), Some("bit varying[]"));
        // The gate still holds for what the suffix wraps.
        assert_eq!(cast("[]"), None);
        assert_eq!(cast("array[]"), None);
        assert_eq!(cast("character varying(5)[]"), None);
        assert_eq!(cast("x\"; drop[]"), None);
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

        // Bare "character" implies a restrictive default modifier
        // (`::character` = char(1) would TRUNCATE a character(3) value), so
        // it casts to the unbounded `bpchar` — which, unlike an uncast text
        // parameter, also compares blank-insensitively in a key clause.
        // Verified against a live server in tests/db_staged.rs.
        // "character varying" is unbounded when bare and keeps its cast.
        let (statements, _) = build_statements(
            &typed_pg_table(),
            &identity(),
            Dialect::Postgres,
            &[
                update(1, "code", Value::Text("xyz".into())),
                update(1, "nick", Value::Text("zed".into())),
            ],
        )
        .unwrap();
        assert_eq!(
            statements[0].sql,
            "UPDATE \"app\".\"typed\" SET \
             \"code\" = $1::bpchar, \"nick\" = $2::character varying \
             WHERE \"id\" = $3::integer"
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
        assert!(
            err.message.contains("key has changed"),
            "message: {}",
            err.message
        );

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
