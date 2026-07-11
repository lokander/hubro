//! Staged-edit model for one table view (FRE-14): pending cell edits, row
//! inserts, and row deletes accumulate here until the user saves (all in one
//! transaction, via [`apply_staged`](crate::db::apply_staged)) or discards.
//!
//! FRE-24/25 (cell editors, insert/delete affordances) only push changes in
//! through [`AppState::stage_cell_edit`](super::state::AppState::stage_cell_edit)
//! / the `stage_insert_*` / `stage_delete` family; everything downstream —
//! dirty rendering, the Save/Discard bar, the transactional apply — already
//! works.
//!
//! Rows are keyed by [`RowLocator::key`], the serialized identity values
//! (`Value` holds `f64` and cannot be a `HashMap` key itself; the key string
//! trades hashability for a lookup through the serialization — see
//! `RowLocator::key` for the exact tradeoffs).

use std::collections::{HashMap, HashSet};

use crate::db::{Dialect, RowLocator, StagedChange, TableMeta, Value};

/// Pending, unapplied changes for one table view.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct TableStage {
    /// Cell edits per row ([`RowLocator::key`] → edits). Edits coalesce per
    /// `(row, column)`: staging a second value for the same cell replaces
    /// the first (last one wins).
    edits: HashMap<String, RowEdits>,
    /// Rows marked for deletion ([`RowLocator::key`] → locator).
    deletes: HashMap<String, RowLocator>,
    /// New rows, in the order they were staged.
    inserts: Vec<PendingInsert>,
    /// Id handed to the next [`Self::add_insert`]; never reused within one
    /// stage, so phantom-row keys stay unambiguous across removals.
    next_insert_id: u64,
    /// A save is in flight; the Save button disables and further saves
    /// no-op until it settles.
    pub saving: bool,
    /// Why the last save failed (named change + database error). The stage
    /// itself stays intact so the user can fix or discard.
    pub last_error: Option<String>,
}

/// Pending cell edits of one existing row.
#[derive(Debug, Clone, PartialEq)]
struct RowEdits {
    locator: RowLocator,
    /// column name → staged value (coalesced, last one wins).
    values: HashMap<String, Value>,
}

/// One pending row insert — a "phantom row" in the grid. Every column
/// starts as "database default"; only columns the user overrides carry a
/// value here. An all-default row saves as `INSERT … DEFAULT VALUES`.
#[derive(Debug, Clone, PartialEq)]
pub struct PendingInsert {
    /// Stage-local id, stable for the life of the stage (ids are never
    /// reused), so the grid can address this phantom row across re-renders —
    /// see [`Self::row_key`].
    id: u64,
    /// column name → staged override. A column absent here is left to the
    /// database (its default, serial/identity assignment, or NULL).
    values: HashMap<String, Value>,
}

impl PendingInsert {
    pub fn id(&self) -> u64 {
        self.id
    }

    /// Grid row key for this phantom row. The `insert:` prefix cannot
    /// collide with [`RowLocator::key`] output (locator keys are sequences
    /// of `<tag><payload>;` where an `i` tag is always followed by digits).
    pub fn row_key(&self) -> String {
        format!("insert:{}", self.id)
    }

    /// The staged override for one column (`None` = database default).
    pub fn value(&self, column: &str) -> Option<&Value> {
        self.values.get(column)
    }

    /// Whether the column still lacks a concrete value: no override, or an
    /// explicit NULL override (which cannot satisfy a required column).
    pub fn lacks_value(&self, column: &str) -> bool {
        self.values.get(column).is_none_or(Value::is_null)
    }

    /// The [`StagedChange`] this insert generates: only overridden columns,
    /// sorted by name (deterministic); no overrides at all → empty columns,
    /// which `apply_staged` renders as `INSERT … DEFAULT VALUES`.
    pub fn change(&self) -> StagedChange {
        let mut columns: Vec<String> = self.values.keys().cloned().collect();
        columns.sort();
        let values = columns.iter().map(|c| self.values[c].clone()).collect();
        StagedChange::Insert { columns, values }
    }
}

impl TableStage {
    /// Stages `column = value` for the row at `locator`, replacing any
    /// earlier edit of the same cell (last one wins). Ignored when the row
    /// is marked for deletion — the delete supersedes cell edits.
    pub fn set_cell_edit(&mut self, locator: RowLocator, column: impl Into<String>, value: Value) {
        let key = locator.key();
        if self.deletes.contains_key(&key) {
            return;
        }
        self.edits
            .entry(key)
            .or_insert_with(|| RowEdits {
                locator,
                values: HashMap::new(),
            })
            .values
            .insert(column.into(), value);
    }

    /// Stages one new all-default row, returning its stage-local id.
    pub fn add_insert(&mut self) -> u64 {
        let id = self.next_insert_id;
        self.next_insert_id += 1;
        self.inserts.push(PendingInsert {
            id,
            values: HashMap::new(),
        });
        id
    }

    /// Stages a concrete value for one column of a pending insert,
    /// replacing any earlier override (last one wins). Unknown ids no-op
    /// (the phantom row may have been removed or applied meanwhile).
    pub fn set_insert_value(&mut self, insert_id: u64, column: impl Into<String>, value: Value) {
        if let Some(insert) = self.inserts.iter_mut().find(|i| i.id == insert_id) {
            insert.values.insert(column.into(), value);
        }
    }

    /// Reverts one column of a pending insert to "database default".
    pub fn clear_insert_value(&mut self, insert_id: u64, column: &str) {
        if let Some(insert) = self.inserts.iter_mut().find(|i| i.id == insert_id) {
            insert.values.remove(column);
        }
    }

    /// Drops a pending insert entirely — deleting a phantom row stages
    /// nothing, the row simply disappears.
    pub fn remove_insert(&mut self, insert_id: u64) {
        self.inserts.retain(|i| i.id != insert_id);
    }

    /// Marks the row at `locator` for deletion. Any pending cell edits of
    /// that row are dropped — the delete supersedes them.
    pub fn mark_delete(&mut self, locator: RowLocator) {
        let key = locator.key();
        self.edits.remove(&key);
        self.deletes.insert(key, locator);
    }

    /// The staged value for one cell, if any (drives dirty-cell rendering:
    /// the grid shows this value with a dirty tint).
    pub fn edited_value(&self, row_key: &str, column: &str) -> Option<&Value> {
        self.edits.get(row_key)?.values.get(column)
    }

    /// Whether the row is marked for deletion (drives dirty-row rendering).
    pub fn is_deleted(&self, row_key: &str) -> bool {
        self.deletes.contains_key(row_key)
    }

    /// Pending rows to insert, in staged order (rendered as phantom rows).
    pub fn inserts(&self) -> &[PendingInsert] {
        &self.inserts
    }

    /// Number of rows staged for deletion (drives the save-time
    /// exact-count confirmation).
    pub fn delete_count(&self) -> usize {
        self.deletes.len()
    }

    /// How many required cells (per [`required_insert_columns`]) are still
    /// unfilled across all pending inserts. Non-zero blocks the Save button.
    pub fn missing_required(&self, required: &HashSet<String>) -> usize {
        self.inserts
            .iter()
            .map(|insert| {
                required
                    .iter()
                    .filter(|column| insert.lacks_value(column))
                    .count()
            })
            .sum()
    }

    /// Number of pending changes as the user counts them: edited cells +
    /// inserts + deletes.
    pub fn pending_count(&self) -> usize {
        let cells: usize = self.edits.values().map(|row| row.values.len()).sum();
        cells + self.inserts.len() + self.deletes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.edits.is_empty() && self.inserts.is_empty() && self.deletes.is_empty()
    }

    /// Removes exactly the given (previously snapshotted, now successfully
    /// applied) changes from the stage, and clears the save bookkeeping
    /// (`saving`, `last_error`). Changes staged AFTER the snapshot survive —
    /// this is what makes staging during an in-flight save safe (see
    /// `AppState::save_staged` for the contract):
    ///
    /// - a cell edit is removed only while its staged value still equals
    ///   the applied value — a re-edit of the same cell after the snapshot
    ///   stays staged;
    /// - an insert is removed by whole-row equality of its *generated
    ///   change* ([`PendingInsert::change`], first match) — an insert whose
    ///   overrides were edited after the snapshot no longer matches and
    ///   stays staged (it will insert a second row, which is what "staging
    ///   more during a save" means for inserts);
    /// - a delete is removed by row — the row is gone, so a delete
    ///   re-staged after the snapshot would be meaningless anyway.
    pub fn remove_applied(&mut self, applied: &[StagedChange]) {
        for change in applied {
            match change {
                StagedChange::Update {
                    locator,
                    column,
                    value,
                } => {
                    let key = locator.key();
                    if let Some(row) = self.edits.get_mut(&key) {
                        if row.values.get(column) == Some(value) {
                            row.values.remove(column);
                        }
                        if row.values.is_empty() {
                            self.edits.remove(&key);
                        }
                    }
                }
                StagedChange::Insert { .. } => {
                    let position = self.inserts.iter().position(|i| i.change() == *change);
                    if let Some(position) = position {
                        self.inserts.remove(position);
                    }
                }
                StagedChange::Delete { locator } => {
                    self.deletes.remove(&locator.key());
                }
            }
        }
        self.saving = false;
        self.last_error = None;
    }

    /// The normalized change list handed to
    /// [`apply_staged`](crate::db::apply_staged):
    ///
    /// 1. updates — rows in row-key order, one change per edited cell,
    ///    columns sorted by name within each row (`apply_staged` groups a
    ///    row's cells into one multi-column UPDATE);
    /// 2. inserts — in staged order;
    /// 3. deletes — in row-key order.
    ///
    /// Sorting makes the order deterministic, so a failure index always
    /// names the same change for the same stage. Coalescing already
    /// happened at staging time, so no `(row, column)` pair repeats.
    pub fn changes(&self) -> Vec<StagedChange> {
        let mut out = Vec::with_capacity(self.pending_count());
        let mut row_keys: Vec<&String> = self.edits.keys().collect();
        row_keys.sort();
        for row_key in row_keys {
            let row = &self.edits[row_key];
            let mut columns: Vec<&String> = row.values.keys().collect();
            columns.sort();
            for column in columns {
                out.push(StagedChange::Update {
                    locator: row.locator.clone(),
                    column: column.clone(),
                    value: row.values[column].clone(),
                });
            }
        }
        for insert in &self.inserts {
            out.push(insert.change());
        }
        let mut delete_keys: Vec<&String> = self.deletes.keys().collect();
        delete_keys.sort();
        for key in delete_keys {
            out.push(StagedChange::Delete {
                locator: self.deletes[key].clone(),
            });
        }
        out
    }
}

/// Columns that must be given a concrete value before a pending insert may
/// save: NOT NULL, no default, and not auto-assigned by the database.
///
/// REQUIRED = `!nullable && default.is_none()`, minus the auto-assigned
/// cases, which differ per backend:
///
/// - **SQLite**: a *single-column* `INTEGER PRIMARY KEY` is an alias for
///   the rowid — auto-assigned on insert even though PRAGMA metadata shows
///   no default. Only the exact (case-insensitive) declared type "INTEGER"
///   aliases the rowid (`INT` or `INTEGER(4)` do not), and only when the
///   PK has exactly one column — an INTEGER member of a composite PK is
///   never auto-assigned. Caveat: in a `WITHOUT ROWID` table an INTEGER PK
///   is not auto-assigned either, but introspection does not expose
///   without-rowid-ness; such a column is (wrongly) treated as
///   auto-assigned here, and leaving it default fails loudly at save time
///   (NOT NULL violation, whole batch rolled back).
/// - **Postgres**: `serial`/`bigserial` columns carry a `nextval(…)`
///   `column_default`, and identity columns (`GENERATED … AS IDENTITY`,
///   which have a NULL `column_default`) are introspected with a
///   `GENERATED … AS IDENTITY` default marker (see `postgres.rs`), so both
///   fall out via `default.is_some()`.
pub fn required_insert_columns(meta: &TableMeta, dialect: Dialect) -> HashSet<String> {
    let single_pk = meta.primary_key().len() == 1;
    meta.columns
        .iter()
        .filter(|column| {
            if column.nullable || column.default.is_some() {
                return false;
            }
            let rowid_alias = dialect == Dialect::Sqlite
                && single_pk
                && column.primary_key_position.is_some()
                && column.type_name.eq_ignore_ascii_case("integer");
            !rowid_alias
        })
        .map(|column| column.name.clone())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn locator(id: i64) -> RowLocator {
        RowLocator {
            identity_values: vec![Value::Integer(id)],
        }
    }

    #[test]
    fn cell_edits_coalesce_last_wins() {
        let mut stage = TableStage::default();
        stage.set_cell_edit(locator(1), "title", Value::Text("first".into()));
        stage.set_cell_edit(locator(1), "title", Value::Text("second".into()));
        assert_eq!(stage.pending_count(), 1);
        assert_eq!(
            stage.edited_value(&locator(1).key(), "title"),
            Some(&Value::Text("second".into()))
        );
        let changes = stage.changes();
        assert_eq!(changes.len(), 1);
        assert_eq!(
            changes[0],
            StagedChange::Update {
                locator: locator(1),
                column: "title".into(),
                value: Value::Text("second".into()),
            }
        );
    }

    #[test]
    fn changes_are_normalized_updates_then_inserts_then_deletes() {
        let mut stage = TableStage::default();
        // Staged deliberately out of order.
        stage.mark_delete(locator(9));
        let insert = stage.add_insert();
        stage.set_insert_value(insert, "a", Value::Integer(1));
        stage.set_cell_edit(locator(2), "b", Value::Integer(20));
        stage.set_cell_edit(locator(2), "a", Value::Integer(10));
        stage.set_cell_edit(locator(1), "a", Value::Integer(5));

        let changes = stage.changes();
        assert_eq!(changes.len(), 5);
        // Updates first: row 1 before row 2 (row-key order), and within
        // row 2 column "a" before "b" (column order).
        assert_eq!(
            changes[0],
            StagedChange::Update {
                locator: locator(1),
                column: "a".into(),
                value: Value::Integer(5),
            }
        );
        assert_eq!(
            changes[1],
            StagedChange::Update {
                locator: locator(2),
                column: "a".into(),
                value: Value::Integer(10),
            }
        );
        assert_eq!(
            changes[2],
            StagedChange::Update {
                locator: locator(2),
                column: "b".into(),
                value: Value::Integer(20),
            }
        );
        assert!(matches!(changes[3], StagedChange::Insert { .. }));
        assert_eq!(
            changes[4],
            StagedChange::Delete {
                locator: locator(9)
            }
        );
    }

    #[test]
    fn delete_supersedes_cell_edits_for_the_row() {
        let mut stage = TableStage::default();
        stage.set_cell_edit(locator(1), "a", Value::Integer(1));
        stage.mark_delete(locator(1));
        // The edit is gone; later edits on the deleted row are ignored.
        stage.set_cell_edit(locator(1), "a", Value::Integer(2));
        assert_eq!(stage.edited_value(&locator(1).key(), "a"), None);
        assert!(stage.is_deleted(&locator(1).key()));
        assert_eq!(stage.pending_count(), 1);
        let changes = stage.changes();
        assert_eq!(changes.len(), 1);
        assert!(matches!(changes[0], StagedChange::Delete { .. }));
    }

    #[test]
    fn empty_stage_reports_empty() {
        let stage = TableStage::default();
        assert!(stage.is_empty());
        assert_eq!(stage.pending_count(), 0);
        assert!(stage.changes().is_empty());

        let mut edited = TableStage::default();
        edited.set_cell_edit(locator(1), "a", Value::Null);
        assert!(!edited.is_empty());
    }

    #[test]
    fn changes_staged_during_a_save_survive_the_success_clear() {
        // Simulates the slow-save race: snapshot the changes (as
        // save_staged does), stage more while the save is "in flight", then
        // apply the success-clear.
        let mut stage = TableStage::default();
        stage.set_cell_edit(locator(1), "a", Value::Integer(1));
        let first_insert = stage.add_insert();
        stage.set_insert_value(first_insert, "a", Value::Integer(9));
        stage.mark_delete(locator(3));
        let snapshot = stage.changes();
        stage.saving = true;

        // Staged after the snapshot: a new cell, a re-edit of the saved
        // cell, and another insert.
        stage.set_cell_edit(locator(2), "b", Value::Integer(2));
        stage.set_cell_edit(locator(1), "a", Value::Integer(100));
        let second_insert = stage.add_insert();
        stage.set_insert_value(second_insert, "a", Value::Integer(10));

        stage.remove_applied(&snapshot);
        assert!(!stage.saving);
        assert!(stage.last_error.is_none());
        // The snapshotted insert and delete are gone; everything staged
        // after the snapshot survives, including the re-edit.
        assert!(!stage.is_deleted(&locator(3).key()));
        assert_eq!(stage.inserts().len(), 1);
        assert_eq!(stage.inserts()[0].value("a"), Some(&Value::Integer(10)));
        assert_eq!(
            stage.edited_value(&locator(1).key(), "a"),
            Some(&Value::Integer(100)),
            "re-edit after the snapshot must not be destroyed"
        );
        assert_eq!(
            stage.edited_value(&locator(2).key(), "b"),
            Some(&Value::Integer(2))
        );
        assert_eq!(stage.pending_count(), 3);
    }

    #[test]
    fn remove_applied_clears_an_untouched_stage_completely() {
        let mut stage = TableStage::default();
        stage.set_cell_edit(locator(1), "a", Value::Integer(1));
        stage.set_cell_edit(locator(1), "b", Value::Null);
        stage.add_insert(); // all-default row → DEFAULT VALUES
        stage.mark_delete(locator(2));
        let snapshot = stage.changes();
        stage.saving = true;
        stage.remove_applied(&snapshot);
        assert!(stage.is_empty());
        assert!(!stage.saving);
    }

    #[test]
    fn insert_change_carries_only_overridden_columns() {
        let mut stage = TableStage::default();
        let insert = stage.add_insert();
        // All-default: empty columns → DEFAULT VALUES downstream.
        assert_eq!(
            stage.inserts()[0].change(),
            StagedChange::Insert {
                columns: vec![],
                values: vec![],
            }
        );
        // Overrides (staged out of name order, last one wins per column;
        // an explicit NULL override is still an override).
        stage.set_insert_value(insert, "title", Value::Text("draft".into()));
        stage.set_insert_value(insert, "title", Value::Text("final".into()));
        stage.set_insert_value(insert, "amount", Value::Integer(5));
        stage.set_insert_value(insert, "note", Value::Null);
        assert_eq!(
            stage.inserts()[0].change(),
            StagedChange::Insert {
                columns: vec!["amount".into(), "note".into(), "title".into()],
                values: vec![Value::Integer(5), Value::Null, Value::Text("final".into())],
            }
        );
        // Revert-to-default drops the override again.
        stage.clear_insert_value(insert, "amount");
        assert_eq!(stage.inserts()[0].value("amount"), None);
        assert_eq!(
            stage.inserts()[0].change(),
            StagedChange::Insert {
                columns: vec!["note".into(), "title".into()],
                values: vec![Value::Null, Value::Text("final".into())],
            }
        );
    }

    #[test]
    fn removing_a_phantom_insert_stages_nothing() {
        let mut stage = TableStage::default();
        let first = stage.add_insert();
        let second = stage.add_insert();
        stage.set_insert_value(second, "a", Value::Integer(1));
        assert_eq!(stage.pending_count(), 2);

        stage.remove_insert(first);
        // Only the other phantom row remains; no delete was staged.
        assert_eq!(stage.inserts().len(), 1);
        assert_eq!(stage.inserts()[0].id(), second);
        assert_eq!(stage.delete_count(), 0);
        assert_eq!(stage.pending_count(), 1);

        stage.remove_insert(second);
        assert!(
            stage.is_empty(),
            "removing the last insert empties the stage"
        );

        // Ids are never reused, so a stale phantom-row key cannot address
        // a newer insert.
        let third = stage.add_insert();
        assert!(third > second);
        // Mutations against removed ids are ignored.
        stage.set_insert_value(second, "a", Value::Integer(9));
        assert_eq!(stage.inserts().len(), 1);
        assert_eq!(stage.inserts()[0].value("a"), None);
    }

    #[test]
    fn phantom_row_keys_are_stable_and_distinct_from_locator_keys() {
        let mut stage = TableStage::default();
        let insert = stage.add_insert();
        let key = stage.inserts()[0].row_key();
        assert_eq!(key, format!("insert:{insert}"));
        stage.set_insert_value(insert, "a", Value::Integer(1));
        assert_eq!(stage.inserts()[0].row_key(), key, "key survives edits");
        // No RowLocator can produce an "insert:…" key (tags are followed by
        // their payload, e.g. `i<digits>;`).
        assert_ne!(locator(0).key(), key);
    }

    fn column(
        name: &str,
        type_name: &str,
        nullable: bool,
        pk: Option<u32>,
        default: Option<&str>,
    ) -> crate::db::ColumnMeta {
        crate::db::ColumnMeta {
            name: name.into(),
            type_name: type_name.into(),
            nullable,
            primary_key_position: pk,
            default: default.map(String::from),
        }
    }

    fn meta(columns: Vec<crate::db::ColumnMeta>) -> TableMeta {
        TableMeta {
            schema: None,
            name: "t".into(),
            kind: crate::db::TableKind::Table,
            columns,
            indexes: vec![],
            foreign_keys: vec![],
        }
    }

    #[test]
    fn required_columns_are_not_null_without_default_or_auto_assignment() {
        let table = meta(vec![
            // SQLite rowid alias: INTEGER single-column PK, auto-assigned.
            column("id", "INTEGER", false, Some(1), None),
            column("title", "TEXT", false, None, None),
            column("note", "TEXT", true, None, None),
            column("state", "TEXT", false, None, Some("'draft'")),
        ]);
        let required = required_insert_columns(&table, Dialect::Sqlite);
        assert_eq!(required, HashSet::from(["title".to_string()]));

        // The declared type must be exactly INTEGER for the rowid alias —
        // "INT" is not auto-assigned; case is ignored.
        let int_pk = meta(vec![column("id", "INT", false, Some(1), None)]);
        assert!(required_insert_columns(&int_pk, Dialect::Sqlite).contains("id"));
        let lower = meta(vec![column("id", "integer", false, Some(1), None)]);
        assert!(required_insert_columns(&lower, Dialect::Sqlite).is_empty());

        // An INTEGER member of a composite PK is never auto-assigned.
        let composite = meta(vec![
            column("a", "INTEGER", false, Some(1), None),
            column("b", "INTEGER", false, Some(2), None),
        ]);
        assert_eq!(
            required_insert_columns(&composite, Dialect::Sqlite),
            HashSet::from(["a".to_string(), "b".to_string()])
        );

        // On Postgres the INTEGER-PK exemption never applies; serial and
        // identity columns are exempt through their default marker instead.
        let pg = meta(vec![
            column("plain_pk", "integer", false, Some(1), None),
            column(
                "serial_id",
                "integer",
                false,
                None,
                Some("nextval('t_id_seq'::regclass)"),
            ),
            column(
                "identity_id",
                "integer",
                false,
                None,
                Some("GENERATED ALWAYS AS IDENTITY"),
            ),
            column("label", "text", false, None, None),
        ]);
        assert_eq!(
            required_insert_columns(&pg, Dialect::Postgres),
            HashSet::from(["plain_pk".to_string(), "label".to_string()])
        );
    }

    #[test]
    fn missing_required_counts_unfilled_cells_across_inserts() {
        let required = HashSet::from(["a".to_string(), "b".to_string()]);
        let mut stage = TableStage::default();
        assert_eq!(
            stage.missing_required(&required),
            0,
            "no inserts, nothing missing"
        );

        let first = stage.add_insert();
        let second = stage.add_insert();
        assert_eq!(stage.missing_required(&required), 4);

        stage.set_insert_value(first, "a", Value::Integer(1));
        // An explicit NULL cannot satisfy a required (NOT NULL) column.
        stage.set_insert_value(first, "b", Value::Null);
        // Overriding a non-required column changes nothing.
        stage.set_insert_value(second, "c", Value::Integer(3));
        assert_eq!(stage.missing_required(&required), 3);

        stage.set_insert_value(first, "b", Value::Integer(2));
        stage.set_insert_value(second, "a", Value::Integer(1));
        stage.set_insert_value(second, "b", Value::Integer(2));
        assert_eq!(stage.missing_required(&required), 0);

        // Edits and deletes never count as missing — only pending inserts.
        stage.set_cell_edit(locator(1), "a", Value::Null);
        stage.mark_delete(locator(2));
        assert_eq!(stage.missing_required(&required), 0);
    }

    #[test]
    fn dirty_lookups_key_on_the_exact_locator() {
        let mut stage = TableStage::default();
        stage.set_cell_edit(locator(1), "a", Value::Integer(1));
        // A different row (or the same digits as text) is not dirty.
        assert!(stage.edited_value(&locator(2).key(), "a").is_none());
        let text_locator = RowLocator {
            identity_values: vec![Value::Text("1".into())],
        };
        assert!(stage.edited_value(&text_locator.key(), "a").is_none());
        // Another column of the edited row is not dirty either.
        assert!(stage.edited_value(&locator(1).key(), "b").is_none());
    }
}
