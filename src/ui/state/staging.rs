//! The staged-edit layer (FRE-14/24/25): staging cell edits, inserts and
//! deletes, the dirty/saving predicates the UI gates on, and the save path
//! with its write-protection confirmation (FRE-111).
//!
//! Staged changes never reach the database until [`AppState::save_staged`]
//! sends them, so this module is the whole boundary between an edit the user
//! can still take back and one they cannot.

use super::*;

impl AppState {
    /// Stages a cell edit (FRE-24 pushes edits in through this). Edits
    /// coalesce per `(row, column)` — the last staged value wins. Staging
    /// is allowed even while a save is in flight; see [`Self::save_staged`]
    /// for the concurrency contract.
    pub fn stage_cell_edit(
        mut self,
        id: ConnectionId,
        table: &TableRef,
        locator: RowLocator,
        column: &str,
        value: Value,
    ) {
        self.nav_guard.set(None);
        self.staged
            .write()
            .entry(id)
            .or_default()
            .entry(table.key())
            .or_default()
            .set_cell_edit(locator, column, value);
    }

    /// Stages a new all-default pending insert — the "+ New row" affordance
    /// (FRE-25). Columns get concrete values via [`Self::stage_insert_value`].
    pub fn stage_insert_row(mut self, id: ConnectionId, table: &TableRef) {
        self.nav_guard.set(None);
        self.staged
            .write()
            .entry(id)
            .or_default()
            .entry(table.key())
            .or_default()
            .add_insert();
    }

    /// Stages a concrete value for one column of a pending insert (last one
    /// wins, like cell edits). No-ops when the phantom row no longer exists.
    pub fn stage_insert_value(
        mut self,
        id: ConnectionId,
        table: &TableRef,
        insert_id: u64,
        column: &str,
        value: Value,
    ) {
        self.nav_guard.set(None);
        if let Some(stage) = self
            .staged
            .write()
            .get_mut(&id)
            .and_then(|tables| tables.get_mut(&table.key()))
        {
            stage.set_insert_value(insert_id, column, value);
        }
    }

    /// Reverts one column of a pending insert to "database default".
    pub fn clear_insert_value(
        mut self,
        id: ConnectionId,
        table: &TableRef,
        insert_id: u64,
        column: &str,
    ) {
        self.nav_guard.set(None);
        if let Some(stage) = self
            .staged
            .write()
            .get_mut(&id)
            .and_then(|tables| tables.get_mut(&table.key()))
        {
            stage.clear_insert_value(insert_id, column);
        }
    }

    /// Removes a pending insert — "deleting" a phantom row stages nothing,
    /// the row just disappears. A stage this empties is cleaned up (so the
    /// Save bar goes away), except while a save is in flight — its
    /// bookkeeping (`saving`, `last_error`) must survive.
    pub fn remove_pending_insert(mut self, id: ConnectionId, table: &TableRef, insert_id: u64) {
        self.nav_guard.set(None);
        let mut staged = self.staged.write();
        let Some(tables) = staged.get_mut(&id) else {
            return;
        };
        let Some(stage) = tables.get_mut(&table.key()) else {
            return;
        };
        stage.remove_insert(insert_id);
        if stage.is_empty() && !stage.saving {
            tables.remove(&table.key());
            if tables.is_empty() {
                staged.remove(&id);
            }
        }
    }

    /// Stages a row delete (FRE-25 pushes deletes in through this).
    pub fn stage_delete(mut self, id: ConnectionId, table: &TableRef, locator: RowLocator) {
        self.nav_guard.set(None);
        self.staged
            .write()
            .entry(id)
            .or_default()
            .entry(table.key())
            .or_default()
            .mark_delete(locator);
    }

    /// The current stage of one table view, if any (cloned for rendering).
    pub fn table_stage(&self, id: ConnectionId, table: &TableRef) -> Option<TableStage> {
        self.staged
            .read()
            .get(&id)
            .and_then(|tables| tables.get(&table.key()))
            .cloned()
    }

    /// Discards all staged changes of one table view. Refused (no-op) while
    /// that table's save is in flight — the running transaction may still
    /// commit, and "discarded" changes silently landing in the database
    /// would be worse than a briefly stuck Discard button.
    pub fn discard_staged(mut self, id: ConnectionId, table: &TableRef) {
        let mut staged = self.staged.write();
        let Some(tables) = staged.get_mut(&id) else {
            return;
        };
        if tables.get(&table.key()).is_some_and(|stage| stage.saving) {
            return;
        }
        tables.remove(&table.key());
        if tables.is_empty() {
            staged.remove(&id);
        }
        drop(staged);
        // A confirmation parked over changes that no longer exist (FRE-111)
        // would otherwise sit there offering to apply nothing.
        self.pending_saves.write().remove(&(id, table.key()));
        self.nav_guard.set(None);
    }

    /// Whether one table view has pending staged changes.
    pub(super) fn stage_dirty(&self, id: ConnectionId, table: &TableRef) -> bool {
        self.staged
            .read()
            .get(&id)
            .and_then(|tables| tables.get(&table.key()))
            .is_some_and(|stage| !stage.is_empty())
    }

    /// Whether one table view has a save in flight.
    pub(super) fn stage_saving(&self, id: ConnectionId, table: &TableRef) -> bool {
        self.staged
            .read()
            .get(&id)
            .and_then(|tables| tables.get(&table.key()))
            .is_some_and(|stage| stage.saving)
    }

    /// Whether any table of the connection has pending staged changes.
    pub(super) fn any_stage_dirty(&self, id: ConnectionId) -> bool {
        self.staged
            .read()
            .get(&id)
            .is_some_and(|tables| tables.values().any(|stage| !stage.is_empty()))
    }

    /// Whether *any* open connection has pending staged edits anywhere. The
    /// window-close guard (FRE-37) uses this: unlike the per-connection
    /// navigation guards, closing the OS window isn't scoped to one connection.
    pub fn any_dirty(&self) -> bool {
        staged_has_dirty(&self.staged.read())
    }

    /// Whether any table of the connection has a save in flight.
    pub(super) fn any_stage_saving(&self, id: ConnectionId) -> bool {
        self.staged
            .read()
            .get(&id)
            .is_some_and(|tables| tables.values().any(|stage| stage.saving))
    }

    /// Runs the two-step guard for one navigation attempt. Returns `true`
    /// when the attempt may proceed: the same intent was parked at least
    /// [`NAV_CONFIRM_MIN_DELAY`] ago. Otherwise parks the intent (first
    /// attempt or a different action) or ignores it (identical repeat
    /// inside the double-click floor — the original park time is kept so a
    /// deliberate later repeat still confirms).
    pub(super) fn nav_guard_allows(mut self, id: ConnectionId, action: NavAction) -> bool {
        let parked = self.nav_guard.read().clone();
        match parked {
            Some(nav) if nav.matches(id, &action) => nav.confirmable(),
            _ => {
                self.nav_guard.set(Some(PendingNav::new(id, action)));
                false
            }
        }
    }

    /// Forces the grid of one table to refetch (used by the grid's Refresh
    /// button and by [`Self::save_staged`] after a successful apply).
    pub fn bump_grid_refresh(mut self, id: ConnectionId, table_key: &str) {
        let mut refresh = self.grid_refresh.write();
        *refresh.entry((id, table_key.to_string())).or_insert(0) += 1;
    }

    /// Applies one table's staged changes in ONE transaction, in the
    /// background. On success the applied changes are removed from the
    /// stage and the grid refetches; on failure the stage stays intact (so
    /// the user can fix or discard) and the Save bar shows which change
    /// failed.
    ///
    /// Concurrency contract (FRE-24/25 rely on this): staging MORE changes
    /// while a save is in flight is allowed — the save snapshots the change
    /// list up front and, on success, removes exactly that snapshot from
    /// the stage ([`TableStage::remove_applied`]), so later edits survive
    /// and keep the Save bar visible. Only a second save and discard are
    /// blocked while `saving` is set.
    /// On a connection marked [`WriteProtection::Confirm`] (FRE-111) the
    /// first call parks the intent in [`Self::pending_saves`] and returns —
    /// the grid then shows a confirmation naming the connection, and
    /// [`Self::confirm_pending_save`] calls back in to do the work. Nothing
    /// is staged, unstaged or sent in the meantime, so dismissing costs the
    /// user nothing.
    pub fn save_staged(mut self, id: ConnectionId, table: &TableRef) {
        let key = (id, table.key());
        // The parked intent is consumed first either way: `save_action`
        // decides whether it still authorizes *these* changes, and a stale
        // one is replaced rather than honoured. Taken before the empty-stage
        // return so an emptied stage can't strand a banner offering to apply
        // nothing.
        let parked = self.pending_saves.write().remove(&key);
        let current = match self.table_stage(id, table) {
            Some(stage) if !stage.is_empty() => stage.changes(),
            _ => return,
        };
        let protection = self.protection_of(id);
        match save_action(protection, parked.as_deref(), &current) {
            SaveAction::Apply => self.apply_staged_now(id, table),
            SaveAction::Park => {
                self.pending_saves.write().insert(key, current);
            }
        }
    }

    /// Confirms the FRE-111 save banner: applies the staged changes.
    ///
    /// Routed through [`Self::save_staged`] rather than applying directly, so
    /// the staleness check runs here too — the Confirm button and a second
    /// Save click must not be able to disagree about what was authorized.
    pub fn confirm_pending_save(self, id: ConnectionId, table: &TableRef) {
        self.save_staged(id, table);
    }

    /// Dismisses the FRE-111 save banner, leaving the staged changes alone.
    pub fn dismiss_pending_save(mut self, id: ConnectionId, table: &TableRef) {
        self.pending_saves.write().remove(&(id, table.key()));
    }

    /// Whether this table's save is waiting on the FRE-111 confirmation.
    pub fn save_awaiting_confirmation(&self, id: ConnectionId, table_key: &str) -> bool {
        self.pending_saves
            .read()
            .contains_key(&(id, table_key.to_string()))
    }

    /// This connection's write protection, defaulting to `Open` for a
    /// connection that is gone.
    fn protection_of(&self, id: ConnectionId) -> WriteProtection {
        self.registry
            .read()
            .get(id)
            .map(|c| c.protection)
            .unwrap_or_default()
    }

    fn apply_staged_now(mut self, id: ConnectionId, table: &TableRef) {
        let table_key = table.key();
        // Snapshot the normalized change list and flip the in-flight flag —
        // one scoped write, nothing spans the await below.
        let changes = {
            let mut staged = self.staged.write();
            let Some(stage) = staged.get_mut(&id).and_then(|t| t.get_mut(&table_key)) else {
                return;
            };
            if stage.saving || stage.is_empty() {
                return;
            }
            stage.saving = true;
            stage.last_error = None;
            stage.changes()
        };
        self.nav_guard.set(None);
        let pool = self.registry.read().get(id).map(|c| c.pool.clone());
        let meta = self.table_meta(id, table);
        let (Some(pool), Some(meta)) = (pool, meta) else {
            self.fail_save(
                id,
                &table_key,
                "connection or schema no longer available".into(),
            );
            return;
        };
        // The same resolution the grid gated its editors on (FRE-87, and the
        // user's marking from FRE-111); if a stage exists here anyway, the
        // failure states the resolver's reason rather than a second,
        // differently-worded one.
        let Some(access) = self.table_access(id, &meta) else {
            self.fail_save(id, &table_key, "connection no longer available".into());
            return;
        };
        let Some(identity) = access.identity.clone().filter(|_| access.can_mutate()) else {
            self.fail_save(
                id,
                &table_key,
                access
                    .read_only_notice()
                    .unwrap_or("This table has no usable row identity.")
                    .to_string(),
            );
            return;
        };
        // spawn_forever: the save must survive the grid unmounting (e.g. a
        // guarded navigation completing while the apply runs).
        spawn_forever(async move {
            let result = apply_staged(&pool, &access, &meta, &identity, &changes).await;
            match result {
                Ok(_counts) => {
                    {
                        // Remove exactly the snapshotted changes: anything
                        // staged after the snapshot survives (and keeps the
                        // Save bar up) instead of being silently destroyed.
                        let mut staged = self.staged.write();
                        if let Some(tables) = staged.get_mut(&id) {
                            if let Some(stage) = tables.get_mut(&table_key) {
                                stage.remove_applied(&changes);
                                if stage.is_empty() {
                                    tables.remove(&table_key);
                                }
                            }
                            if tables.is_empty() {
                                staged.remove(&id);
                            }
                        }
                    }
                    self.bump_grid_refresh(id, &table_key);
                }
                Err(err) => {
                    // Name the failing change so the user can find it (for a
                    // grouped update: the row and its columns).
                    let message = match (err.change_index, &err.change_summary) {
                        (Some(index), Some(summary)) => format!(
                            "change {} of {} ({summary}) failed: {} — nothing was applied",
                            index + 1,
                            changes.len(),
                            err.message
                        ),
                        // No index: the transaction itself failed to open or
                        // commit, so there is no rollback guarantee to claim.
                        _ => format!(
                            "{} — the batch may or may not have been applied; \
                             refresh to see the current state",
                            err.message
                        ),
                    };
                    self.fail_save(id, &table_key, message);
                }
            }
        });
    }

    /// Records a failed save on the stage (kept intact) and re-enables Save.
    fn fail_save(mut self, id: ConnectionId, table_key: &str, message: String) {
        let mut staged = self.staged.write();
        if let Some(stage) = staged.get_mut(&id).and_then(|t| t.get_mut(table_key)) {
            stage.saving = false;
            stage.last_error = Some(message);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn staged_has_dirty_flags_any_pending_edit_anywhere() {
        use crate::db::RowLocator;
        let mut registry = ConnectionRegistry::default();
        let pool =
            DbPool::Sqlite(sqlx::sqlite::SqlitePool::connect_lazy("sqlite::memory:").unwrap());
        let id = registry.insert("t.db", pool, WriteProtection::Open);

        // Empty map: nothing to lose.
        let mut staged: HashMap<ConnectionId, HashMap<String, TableStage>> = HashMap::new();
        assert!(!staged_has_dirty(&staged));

        // A present-but-empty stage still isn't dirty (empties are usually
        // pruned, but the guard must not trip on a lingering one).
        let mut tables: HashMap<String, TableStage> = HashMap::new();
        tables.insert("public.t".to_string(), TableStage::default());
        staged.insert(id, tables);
        assert!(!staged_has_dirty(&staged));

        // One pending delete anywhere makes the whole app dirty.
        staged
            .get_mut(&id)
            .unwrap()
            .get_mut("public.t")
            .unwrap()
            .mark_delete(RowLocator {
                identity_values: vec![crate::db::Value::Integer(1)],
            });
        assert!(staged_has_dirty(&staged));
    }

    #[tokio::test]
    async fn nav_guard_confirm_needs_a_matching_intent_and_the_time_floor() {
        // A registry-issued id (ConnectionId is opaque outside db).
        let mut registry = ConnectionRegistry::default();
        let pool =
            DbPool::Sqlite(sqlx::sqlite::SqlitePool::connect_lazy("sqlite::memory:").unwrap());
        let id = registry.insert("t.db", pool, WriteProtection::Open);

        let action = NavAction::CloseConnection;
        let fresh = PendingNav::new(id, action.clone());
        assert!(fresh.matches(id, &action));
        assert!(
            !fresh.confirmable(),
            "an immediate identical repeat (double-click) must not confirm"
        );
        let aged = PendingNav {
            parked_at: Instant::now() - NAV_CONFIRM_MIN_DELAY,
            ..fresh
        };
        assert!(aged.confirmable(), "a deliberate later repeat confirms");
        assert!(
            !aged.matches(id, &NavAction::SetPane(Pane::Sql)),
            "a different action never confirms a parked intent"
        );
    }

    #[test]
    fn a_confirm_marked_connection_parks_the_first_save_and_applies_the_confirmed_one() {
        let changes = vec![delete(1)];
        // First click: nothing parked yet, so the prompt goes up.
        assert_eq!(
            save_action(WriteProtection::Confirm, None, &changes),
            SaveAction::Park
        );
        // Second click, stage unchanged: the confirmation stands.
        assert_eq!(
            save_action(WriteProtection::Confirm, Some(&changes), &changes),
            SaveAction::Apply
        );
    }

    #[test]
    fn a_confirmation_goes_stale_when_the_stage_moves_on() {
        // The hole this closes: a parked confirmation named ONE change. If the
        // stage grows, shrinks or changes before the user clicks through,
        // applying without re-prompting would write something they never read.
        let confirmed = vec![delete(1)];
        for moved_on in [
            vec![delete(1), delete(2)], // staged more
            vec![delete(2)],            // swapped for a different row
            vec![],                     // discarded
        ] {
            assert_eq!(
                save_action(WriteProtection::Confirm, Some(&confirmed), &moved_on),
                SaveAction::Park,
                "{moved_on:?} must send the user back through the prompt"
            );
        }
    }

    #[test]
    fn an_unmarked_connection_never_parks_a_save() {
        let changes = vec![delete(1)];
        assert_eq!(
            save_action(WriteProtection::Open, None, &changes),
            SaveAction::Apply,
            "Open must behave exactly as it did before FRE-111"
        );
    }

    #[test]
    fn a_read_only_marking_does_not_prompt_because_there_is_nothing_to_confirm() {
        // The write is refused underneath by the capability resolution; a
        // prompt here would have only one possible outcome — failure.
        let changes = vec![delete(1)];
        assert_eq!(
            save_action(WriteProtection::ReadOnly, None, &changes),
            SaveAction::Apply
        );
    }

    fn delete(id: i64) -> StagedChange {
        StagedChange::Delete {
            locator: crate::db::RowLocator {
                identity_values: vec![Value::Integer(id)],
            },
        }
    }
}
