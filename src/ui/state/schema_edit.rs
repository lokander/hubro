//! Running one schema edit in the background (FRE-122).
//!
//! The same shape as the import task next door: `spawn_forever` so a pane
//! switch cannot cancel a statement mid-flight, a per-connection status signal,
//! and a generation counter so a slow edit cannot record its outcome over one
//! started after it.
//!
//! Two things are specific to a *schema* change, and both are about what the
//! rest of the app is still showing afterwards:
//!
//! - **The schema is re-introspected**, because everything downstream of it —
//!   the sidebar, the schema pane, the editor's completions — was built from
//!   the shape the statement just changed.
//! - **The selection is repointed** ([`AfterEdit`]). A renamed table under the
//!   old name, or a dropped one, leaves the pane sitting on an object that no
//!   longer exists; following the rename is what makes the edit feel like it
//!   happened to the thing on screen rather than to the database in general.
//!
//! The statement itself goes through [`run_script`] exactly as a typed one
//! does. That is deliberate and is the whole safety argument: the text that
//! reaches the server is checked against the connection's effective
//! capabilities by [`script_refusal`](crate::db::script_refusal) at the moment
//! it runs, so an edit typed into the dialog's box cannot do what the button
//! that generated it would have been refused for.

use super::*;

/// Progress of the most recent schema edit on one connection.
#[derive(Debug, Clone, PartialEq)]
pub enum SchemaEditStatus {
    Running {
        /// What is being done, e.g. "Dropping table orders" — the dialog's
        /// button and the pane's status line say the same thing.
        label: String,
    },
    Done {
        label: String,
    },
    Failed(String),
}

impl SchemaEditStatus {
    /// The pane's status line: display text plus a Tailwind colour class,
    /// mirroring [`ImportStatus::line`].
    pub fn line(&self) -> (String, &'static str) {
        match self {
            SchemaEditStatus::Running { label } => {
                (format!("{label}…"), "text-slate-500 dark:text-slate-400")
            }
            SchemaEditStatus::Done { label } => {
                (label.clone(), "text-emerald-700 dark:text-emerald-400")
            }
            SchemaEditStatus::Failed(err) => {
                (format!("Failed: {err}"), "text-red-600 dark:text-red-400")
            }
        }
    }

    /// The error of a failed edit, for the dialog — which keeps the statement
    /// on screen so it can be corrected and run again.
    pub fn error(&self) -> Option<&str> {
        match self {
            SchemaEditStatus::Failed(err) => Some(err),
            _ => None,
        }
    }

    pub fn is_running(&self) -> bool {
        matches!(self, SchemaEditStatus::Running { .. })
    }
}

/// What the edit means for the table the tab currently has selected.
///
/// Derived by [`after_edit`] from the operation *and whether its SQL was
/// edited*, never from the operation alone: the box is editable, so a rename
/// whose target the user retyped would send the selection to a name that was
/// never created.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AfterEdit {
    /// Leave the selection alone. Also what an edited statement gets: what it
    /// did is not known here, and the reloaded schema is the honest answer.
    Keep,
    /// The selected table now goes by this name.
    Follow(TableRef),
    /// The selected table is gone.
    Deselect,
}

/// What should happen to the selection after `op` succeeds.
///
/// `edited` is whether the statement that ran differs from the one `op`
/// generated. When it does, the answer is always [`AfterEdit::Keep`]: the
/// dialog knows which *button* was pressed, but the text is what ran, and
/// following a rename to a name the user replaced would point the pane at
/// something that does not exist — the exact failure this is meant to avoid.
pub fn after_edit(op: &SchemaOp, edited: bool, table: &TableRef) -> AfterEdit {
    if edited {
        return AfterEdit::Keep;
    }
    match op {
        SchemaOp::RenameTable { new_name } => AfterEdit::Follow(TableRef {
            // A rename leaves the table in its schema on every dialect here —
            // none of the generated forms takes a qualified target.
            schema: table.schema.clone(),
            name: new_name.trim().to_string(),
        }),
        SchemaOp::DropTable => AfterEdit::Deselect,
        _ => AfterEdit::Keep,
    }
}

/// Everything one schema edit needs, assembled by the dialog and handed over
/// in one piece.
#[derive(Debug, Clone, PartialEq)]
pub struct SchemaEditRequest {
    /// The table the edit was started from — what the selection is repointed
    /// relative to, and whose grid is refreshed.
    pub table: TableRef,
    /// The statement as it stands in the dialog's box: generated, possibly
    /// edited. This, not the operation, is what runs.
    pub sql: String,
    /// Present-tense description for the status line ("Dropping table orders").
    pub running_label: String,
    /// Past-tense description for the same line once it succeeds.
    pub done_label: String,
    pub after: AfterEdit,
}

impl AppState {
    /// Runs one schema edit in the background. The outcome lands in
    /// [`Self::schema_edits`]; the dialog watches it and the pane shows the
    /// line.
    ///
    /// Capabilities are resolved **here**, from the live connection, for the
    /// same reason the import resolves them at start rather than at open: the
    /// user can mark a connection read-only while a dialog is sitting open, and
    /// the answer that matters is the one at the moment the statement runs.
    pub fn start_schema_edit(mut self, id: ConnectionId, request: SchemaEditRequest) {
        let connection = self
            .registry
            .read()
            .get(id)
            .map(|c| (c.pool.clone(), c.capabilities()));
        let Some((pool, caps)) = connection else {
            let generation = self.begin_schema_edit(id, &request.running_label);
            self.finish_schema_edit(id, generation, Err("connection closed".into()), &request);
            return;
        };
        let statements = split_statements(&request.sql, pool.dialect());
        if statements.is_empty() {
            // Reachable past the dialog's own blank check: `-- nothing` and
            // `;` are not blank, and `split_statements` skips comment-only and
            // empty statements. Reported rather than returned silently — a
            // press that does nothing at all is indistinguishable from one
            // that failed to register.
            let generation = self.begin_schema_edit(id, &request.running_label);
            self.finish_schema_edit(id, generation, Err(NOTHING_TO_RUN.into()), &request);
            return;
        }
        let generation = self.begin_schema_edit(id, &request.running_label);
        // spawn_forever: the statement must survive the dialog unmounting. A
        // plain spawn would drop the future mid-DDL — which on a backend
        // without transactional DDL is not a rollback, just an unobserved
        // outcome.
        let task = spawn_forever(async move {
            let outcome = run_script(&pool, caps, &statements, |_| {})
                .await
                .map_err(|err| err.error.to_string());
            self.finish_schema_edit(id, generation, outcome, &request);
        });
        self.schema_edit_tasks.write().insert(id, task);
    }

    /// Marks the connection's schema-edit slot Running and returns its
    /// generation.
    fn begin_schema_edit(mut self, id: ConnectionId, label: &str) -> u64 {
        let generation = {
            let mut generations = self.schema_edit_generations.write();
            let entry = generations.entry(id).or_insert(0);
            *entry += 1;
            *entry
        };
        self.schema_edits.write().insert(
            id,
            SchemaEditStatus::Running {
                label: label.to_string(),
            },
        );
        generation
    }

    /// Records the outcome — unless a newer edit owns the slot — and, on
    /// success, brings everything the statement invalidated back into line: the
    /// schema itself, the grid's page, and the selection.
    fn finish_schema_edit(
        mut self,
        id: ConnectionId,
        generation: u64,
        outcome: Result<(), String>,
        request: &SchemaEditRequest,
    ) {
        if self.schema_edit_generations.read().get(&id).copied() != Some(generation) {
            return;
        }
        self.schema_edit_tasks.write().remove(&id);
        let status = match outcome {
            Ok(()) => {
                self.apply_after_edit(id, request);
                SchemaEditStatus::Done {
                    label: request.done_label.clone(),
                }
            }
            Err(err) => SchemaEditStatus::Failed(err),
        };
        self.schema_edits.write().insert(id, status);
    }

    /// Re-reads the schema and repoints what the statement moved out from
    /// under the view.
    ///
    /// The staged edits of a table that was dropped or renamed are discarded
    /// first, and before the selection moves: they address rows by a name that
    /// no longer resolves, so saving them would either fail or — after a rename
    /// — write to whatever now answers to the old name.
    fn apply_after_edit(mut self, id: ConnectionId, request: &SchemaEditRequest) {
        match &request.after {
            AfterEdit::Keep => {}
            AfterEdit::Follow(new_table) => {
                self.discard_staged(id, &request.table);
                if let Some(ui) = self.tab_ui.write().get_mut(&id) {
                    if ui.selected_table.as_ref() == Some(&request.table) {
                        ui.selected_table = Some(new_table.clone());
                    }
                }
            }
            AfterEdit::Deselect => {
                self.discard_staged(id, &request.table);
                if let Some(ui) = self.tab_ui.write().get_mut(&id) {
                    if ui.selected_table.as_ref() == Some(&request.table) {
                        ui.selected_table = None;
                    }
                }
            }
        }
        // The rows on screen were fetched before the statement ran — a
        // truncate or an added column leaves the grid showing a page that no
        // longer describes the table.
        self.bump_grid_refresh(id, &request.table.key());
        self.load_schema(id);
    }

    /// Clears the schema-edit line for one connection — the pane's dismiss.
    pub fn clear_schema_edit_status(mut self, id: ConnectionId) {
        self.schema_edits.write().remove(&id);
    }

    /// This connection's schema-edit status, if any.
    pub fn schema_edit_status(&self, id: ConnectionId) -> Option<SchemaEditStatus> {
        self.schema_edits.read().get(&id).cloned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table_ref(schema: Option<&str>, name: &str) -> TableRef {
        TableRef {
            schema: schema.map(str::to_string),
            name: name.into(),
        }
    }

    #[test]
    fn an_unedited_rename_takes_the_selection_with_it() {
        let table = table_ref(Some("app"), "orders");
        assert_eq!(
            after_edit(
                &SchemaOp::RenameTable {
                    new_name: "  invoices  ".into()
                },
                false,
                &table
            ),
            // Trimmed to match the identifier that was actually generated, and
            // still in the schema it was in.
            AfterEdit::Follow(table_ref(Some("app"), "invoices"))
        );
    }

    #[test]
    fn an_unedited_drop_clears_the_selection() {
        assert_eq!(
            after_edit(&SchemaOp::DropTable, false, &table_ref(None, "t")),
            AfterEdit::Deselect
        );
    }

    #[test]
    fn an_edited_statement_never_moves_the_selection() {
        // The box is editable, and the button pressed says nothing about what
        // the text does. Following a rename whose target was retyped would
        // point the pane at a table that was never created — worse than the
        // "no longer in the schema" state, because it looks like it worked.
        let table = table_ref(Some("app"), "orders");
        for op in [
            SchemaOp::RenameTable {
                new_name: "invoices".into(),
            },
            SchemaOp::DropTable,
        ] {
            assert_eq!(after_edit(&op, true, &table), AfterEdit::Keep, "{op:?}");
        }
    }

    #[test]
    fn operations_that_keep_the_table_leave_the_selection_alone() {
        let table = table_ref(None, "t");
        for op in [
            SchemaOp::Truncate,
            SchemaOp::AddColumn {
                name: "c".into(),
                type_name: "text".into(),
            },
            SchemaOp::RenameColumn {
                column: "a".into(),
                new_name: "b".into(),
            },
            SchemaOp::DropIndex { name: "i".into() },
            SchemaOp::CreateIndex {
                name: "i".into(),
                columns: vec!["a".into()],
                unique: false,
            },
        ] {
            assert_eq!(after_edit(&op, false, &table), AfterEdit::Keep, "{op:?}");
        }
    }

    #[test]
    fn the_status_line_distinguishes_running_from_done_from_failed() {
        let running = SchemaEditStatus::Running {
            label: "Dropping table orders".into(),
        };
        assert!(running.is_running());
        assert_eq!(running.line().0, "Dropping table orders…");
        assert_eq!(running.error(), None);

        let done = SchemaEditStatus::Done {
            label: "Dropped table orders".into(),
        };
        assert!(!done.is_running());
        assert_eq!(done.line().0, "Dropped table orders");
        assert!(done.line().1.contains("emerald"));

        let failed = SchemaEditStatus::Failed("no such table".into());
        assert_eq!(failed.error(), Some("no such table"));
        assert!(failed.line().1.contains("red"));
    }
}
