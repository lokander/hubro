//! Staged-edit model for one table view (FRE-14): pending cell edits, row
//! inserts, and row deletes accumulate here until the user saves (all in one
//! transaction, via [`apply_staged`](crate::db::apply_staged)) or discards.
//!
//! FRE-24/25 (cell editors, insert/delete affordances) only push changes in
//! through [`AppState::stage_cell_edit`](super::state::AppState::stage_cell_edit)
//! / `stage_insert` / `stage_delete`; everything downstream — dirty
//! rendering, the Save/Discard bar, the transactional apply — already works.
//!
//! Rows are keyed by [`RowLocator::key`], the serialized identity values
//! (`Value` holds `f64` and cannot be a `HashMap` key itself; the key string
//! trades hashability for a lookup through the serialization — see
//! `RowLocator::key` for the exact tradeoffs).

use std::collections::HashMap;

use crate::db::{RowLocator, StagedChange, Value};

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

/// One pending row insert. `columns` empty means "all defaults".
#[derive(Debug, Clone, PartialEq)]
pub struct PendingInsert {
    pub columns: Vec<String>,
    pub values: Vec<Value>,
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

    /// Stages one new row.
    pub fn add_insert(&mut self, columns: Vec<String>, values: Vec<Value>) {
        self.inserts.push(PendingInsert { columns, values });
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

    /// Number of pending changes as the user counts them: edited cells +
    /// inserts + deletes.
    pub fn pending_count(&self) -> usize {
        let cells: usize = self.edits.values().map(|row| row.values.len()).sum();
        cells + self.inserts.len() + self.deletes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.edits.is_empty() && self.inserts.is_empty() && self.deletes.is_empty()
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
            out.push(StagedChange::Insert {
                columns: insert.columns.clone(),
                values: insert.values.clone(),
            });
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
        stage.add_insert(vec!["a".into()], vec![Value::Integer(1)]);
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
