//! Running SQL and getting results out: the editor buffer, the script run
//! with its write-confirmation gate and cancellation, query-history
//! recording, and the CSV/JSON exports.
//!
//! Split out of [`super`] for the same reason as [`super::connect`]: it is
//! procedural orchestration over [`AppState`]'s signals — spawn a task, track
//! a generation, land the outcome — not state definition.

use super::*;

impl AppState {
    /// Stores the editor buffer (synced from the webview on change). An
    /// actual text change invalidates any pending write confirmation — the
    /// banner must never run SQL that no longer matches the buffer.
    pub fn set_sql_text(mut self, id: ConnectionId, text: String) {
        let changed = {
            let mut tab_ui = self.tab_ui.write();
            let ui = tab_ui.entry(id).or_default();
            let changed = ui.sql_text != text;
            ui.sql_text = text;
            changed
        };
        if changed {
            self.pending_sql.write().remove(&id);
        }
    }

    /// Runs a free-form SQL script against one connection. Scripts where
    /// any statement can mutate the database (see [`needs_confirmation`])
    /// are not executed yet: they are stashed in [`Self::pending_sql`] and
    /// the editor shows a confirmation banner.
    ///
    /// A statement the connection's capabilities forbid (FRE-87) is refused
    /// here, *before* the confirmation banner — being asked to confirm a
    /// write that can never run is a prompt with no right answer.
    pub fn run_sql(mut self, id: ConnectionId, sql: String) {
        self.pending_sql.write().remove(&id);
        // Effective capabilities: the backend's, narrowed by the user's
        // marking (FRE-111). Reading `pool.backend_capabilities()` here instead would
        // let a script write to a connection marked read-only.
        let (dialect, caps) = match self.registry.read().get(id) {
            Some(connection) => (connection.pool.dialect(), connection.capabilities()),
            None => return, // connection closed underneath the editor
        };
        let statements = split_statements(&sql, dialect);
        if statements.is_empty() {
            return;
        }
        if let Some((statement_index, reason)) = script_refusal(caps, &statements, dialect) {
            // Claim the run slot like a real run does: an in-flight script
            // must not push its results into the refusal or overwrite it.
            self.claim_run_slot(id);
            self.sql_runs.write().insert(
                id,
                SqlRun {
                    statements: Vec::new(),
                    status: RunStatus::Refused {
                        reason: reason.to_string(),
                        statement_index,
                        preview: statement_preview(&statements[statement_index]),
                    },
                },
            );
            return;
        }
        if statements.iter().any(|s| needs_confirmation(s, dialect)) {
            self.pending_sql.write().insert(
                id,
                PendingSql {
                    script: sql,
                    statements,
                },
            );
            return;
        }
        self.execute_script(id, sql, statements);
    }

    /// Confirms the write banner: runs the stashed script.
    pub fn confirm_pending_sql(mut self, id: ConnectionId) {
        let pending = self.pending_sql.write().remove(&id);
        if let Some(pending) = pending {
            self.execute_script(id, pending.script, pending.statements);
        }
    }

    /// Dismisses the write banner without running anything.
    pub fn dismiss_pending_sql(mut self, id: ConnectionId) {
        self.pending_sql.write().remove(&id);
    }

    /// Aborts the in-flight run, keeping the outcomes of the statements
    /// that already finished visible and marking the run cancelled.
    ///
    /// Cancelling only drops the sqlx future — on BOTH backends the
    /// in-flight statement itself is NOT interrupted and still completes
    /// (a cancelled UPDATE still commits):
    /// - SQLite: the statement runs to completion on sqlx's worker thread;
    ///   only the future stops being polled.
    /// - Postgres: returning the pooled connection makes sqlx drain the
    ///   socket until the in-flight query has finished server-side, after
    ///   which the connection goes back to the pool healthy — the server
    ///   never sees a disconnect, so the query is not aborted.
    ///
    /// Either way each cancelled long-running query pins one pool
    /// connection until the statement finishes.
    pub fn cancel_sql(mut self, id: ConnectionId) {
        let task = self.sql_tasks.write().remove(&id);
        let Some(task) = task else { return };
        task.cancel();
        if let Some(run) = self.sql_runs.write().get_mut(&id) {
            if run.status == RunStatus::Running {
                run.status = RunStatus::Cancelled;
            }
        }
    }

    /// Takes ownership of this connection's SQL run slot: cancels any
    /// still-running task and bumps the generation, so a run that completes
    /// later can tell it has been superseded and leaves the new result
    /// alone. Returns the new generation.
    fn claim_run_slot(&mut self, id: ConnectionId) -> u64 {
        let previous = self.sql_tasks.write().remove(&id);
        if let Some(previous) = previous {
            previous.cancel();
        }
        let mut generations = self.sql_generations.write();
        let entry = generations.entry(id).or_insert(0);
        *entry += 1;
        *entry
    }

    /// Executes a split script in the background: reads fetch rows, writes
    /// report affected counts, execution stops at the first error. Each
    /// statement's outcome lands in [`Self::sql_runs`] as it finishes.
    fn execute_script(mut self, id: ConnectionId, script: String, statements: Vec<String>) {
        let Some((pool, caps)) = self
            .registry
            .read()
            .get(id)
            .map(|c| (c.pool.clone(), c.capabilities()))
        else {
            return;
        };
        let generation = self.claim_run_slot(id);
        self.sql_runs.write().insert(
            id,
            SqlRun {
                statements: Vec::new(),
                status: RunStatus::Running,
            },
        );
        // spawn_forever: the run must survive pane/tab switches unmounting
        // the editor component. No signal borrow is held across an await —
        // the pool is cloned out above and every write below is scoped.
        let task = spawn_forever(async move {
            let started = std::time::Instant::now();
            let result = run_script(&pool, caps, &statements, |statement| {
                if self.sql_generation(id) == generation {
                    if let Some(run) = self.sql_runs.write().get_mut(&id) {
                        run.statements.push(SharedStatement::new(statement));
                    }
                }
            })
            .await;
            let elapsed_ms = started.elapsed().as_millis() as u64;
            // History is recorded even when a newer run made this one stale —
            // the script did execute. Cancelled runs never reach this point
            // (the future is dropped), so they are not recorded. Recorded
            // fire-and-forget: a wedged history.db must never delay the
            // status update below (FRE-72).
            let error_text = result.as_ref().err().map(|e| e.error.to_string());
            let success = result.is_ok();
            spawn_forever(async move {
                self.record_history(id, script, success, error_text).await;
            });
            // Stale-run guard: a newer run (or a close) owns the slot now.
            if self.sql_generation(id) != generation {
                return;
            }
            self.sql_tasks.write().remove(&id);
            if let Some(run) = self.sql_runs.write().get_mut(&id) {
                run.status = match result {
                    Ok(()) => RunStatus::Done { elapsed_ms },
                    Err(err) => RunStatus::Failed {
                        error: err.error.to_string(),
                        statement_index: err.statement_index,
                        preview: err.preview,
                        elapsed_ms,
                        rollback: err.rollback,
                    },
                };
            }
        });
        self.sql_tasks.write().insert(id, task);
    }

    fn sql_generation(self, id: ConnectionId) -> u64 {
        self.sql_generations.read().get(&id).copied().unwrap_or(0)
    }

    /// Best-effort history write for a completed run: never blocks or fails
    /// the run itself. All signal reads are scoped before the await; the
    /// nonce bump afterwards tells open history panels to re-query. A write
    /// failure surfaces in the history panel via [`Self::history_error`]
    /// (and clears again on the next successful write).
    async fn record_history(
        mut self,
        id: ConnectionId,
        script: String,
        success: bool,
        error: Option<String>,
    ) {
        let locator = self
            .open_locators
            .read()
            .iter()
            .find(|(open_id, _)| *open_id == id)
            .map(|(_, locator)| locator.clone());
        let Some(locator) = locator else { return };
        let store = self.history.read().clone();
        let Some(store) = store else { return };
        match store
            .record(&locator, &script, success, error.as_deref())
            .await
        {
            Ok(recorded) => {
                // The store answered, so any shown record failure is stale —
                // whether this run was recorded (true) or recording is
                // switched off (false).
                self.history_record_error.set(None);
                if recorded {
                    let mut nonce = self.history_nonce.write();
                    *nonce += 1;
                }
            }
            Err(err) => {
                self.history_record_error
                    .set(Some(format!("Query ran, but recording it failed: {err}")));
            }
        }
    }

    /// Persists the history opt-out flag (best-effort) and mirrors it into
    /// the UI signal immediately.
    pub fn set_history_recording(mut self, enabled: bool) {
        self.history_recording.set(enabled);
        let store = self.history.read().clone();
        let Some(store) = store else { return };
        spawn_forever(async move {
            let _ = store.set_recording(enabled).await;
        });
    }

    /// Deletes one connection's history and refreshes open panels.
    pub fn clear_history(self, id: ConnectionId) {
        let locator = self
            .open_locators
            .read()
            .iter()
            .find(|(open_id, _)| *open_id == id)
            .map(|(_, locator)| locator.clone());
        let Some(locator) = locator else { return };
        let store = self.history.read().clone();
        let Some(store) = store else { return };
        let mut nonce_signal = self.history_nonce;
        spawn_forever(async move {
            if store.clear(&locator).await.is_ok() {
                let mut nonce = nonce_signal.write();
                *nonce += 1;
            }
        });
    }

    /// Streams a live query (the current grid view: filter + sort, no paging)
    /// to `path` in `format`, in a background task. The query re-runs against
    /// the connection so it always reflects committed data; rows are pulled
    /// one at a time and written incrementally (see
    /// [`DbPool::export`](crate::db::DbPool::export)). Progress lands in
    /// [`Self::export_status`]; the UI never blocks.
    pub fn export_query(
        mut self,
        id: ConnectionId,
        sql: String,
        params: Vec<Value>,
        format: ExportFormat,
        path: PathBuf,
    ) {
        let slot = (id, ExportPane::Grid);
        let pool = self.registry.read().get(id).map(|c| c.pool.clone());
        let Some(pool) = pool else {
            self.begin_export(slot);
            self.export_status
                .write()
                .insert(slot, ExportStatus::Failed("connection closed".into()));
            return;
        };
        let generation = self.begin_export(slot);
        // spawn_forever: the export must survive the grid unmounting (a pane
        // or tab switch) — a plain spawn would cancel it mid-write.
        spawn_forever(async move {
            use std::io::Write as _;
            // Stream into a temp file and rename on success, so a mid-stream
            // failure can't clobber an existing file at `path`.
            let tmp = export_temp_path(&path);
            let outcome = async {
                let file = std::fs::File::create(&tmp).map_err(|e| e.to_string())?;
                let mut writer = std::io::BufWriter::new(file);
                let rows = pool
                    .export(&sql, &params, format, &mut writer)
                    .await
                    .map_err(|e| e.to_string())?;
                writer.flush().map_err(|e| e.to_string())?;
                drop(writer);
                std::fs::rename(&tmp, &path).map_err(|e| e.to_string())?;
                Ok::<u64, String>(rows)
            }
            .await;
            if outcome.is_err() {
                let _ = std::fs::remove_file(&tmp);
            }
            self.finish_export(slot, generation, outcome);
        });
    }

    /// Writes an already-materialized [`QueryResult`] (the SQL editor's held
    /// result) to `path` in `format`, in a background task. Shares the row
    /// formatters with [`Self::export_query`]; no database round-trip.
    pub fn export_result(
        self,
        id: ConnectionId,
        result: QueryResult,
        format: ExportFormat,
        path: PathBuf,
    ) {
        let slot = (id, ExportPane::Sql);
        let generation = self.begin_export(slot);
        spawn_forever(async move {
            use std::io::Write as _;
            let tmp = export_temp_path(&path);
            let outcome = (|| {
                let file = std::fs::File::create(&tmp).map_err(|e| e.to_string())?;
                let mut writer = std::io::BufWriter::new(file);
                let rows = write_result(&result, format, &mut writer).map_err(|e| e.to_string())?;
                writer.flush().map_err(|e| e.to_string())?;
                drop(writer);
                std::fs::rename(&tmp, &path).map_err(|e| e.to_string())?;
                Ok::<u64, String>(rows)
            })();
            if outcome.is_err() {
                let _ = std::fs::remove_file(&tmp);
            }
            self.finish_export(slot, generation, outcome);
        });
    }

    /// Marks a slot Running and returns the new export's generation.
    fn begin_export(mut self, slot: (ConnectionId, ExportPane)) -> u64 {
        let generation = {
            let mut generations = self.export_generations.write();
            let entry = generations.entry(slot).or_insert(0);
            *entry += 1;
            *entry
        };
        self.export_status
            .write()
            .insert(slot, ExportStatus::Running);
        generation
    }

    /// Records an export's terminal status — unless a newer export owns the
    /// slot, in which case this outcome is stale and dropped.
    fn finish_export(
        mut self,
        slot: (ConnectionId, ExportPane),
        generation: u64,
        outcome: Result<u64, String>,
    ) {
        let latest = self.export_generations.read().get(&slot).copied();
        if latest != Some(generation) {
            return;
        }
        let status = match outcome {
            Ok(rows) => ExportStatus::Done { rows },
            Err(err) => ExportStatus::Failed(err),
        };
        self.export_status.write().insert(slot, status);
    }
}

/// Sibling temp path for an atomic export write (`foo.csv` → `foo.csv.part`).
/// Streaming into this and renaming on success keeps a mid-stream failure
/// from clobbering an existing file at the destination.
fn export_temp_path(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".part");
    path.with_file_name(name)
}
