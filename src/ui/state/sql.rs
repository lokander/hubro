//! Running SQL and getting results out: the editor buffers, the script run
//! with its write-confirmation gate and cancellation, query-history
//! recording, saved queries (FRE-113), and the CSV/JSON exports.
//!
//! Split out of [`super`] for the same reason as [`super::connect`]: it is
//! procedural orchestration over [`AppState`]'s signals — spawn a task, track
//! a generation, land the outcome — not state definition.

use super::*;

impl AppState {
    /// One tab's SQL buffers in tab order (FRE-113). A tab that has never
    /// been to its SQL pane has no entry yet; it reads as the one scratch
    /// buffer such a pane shows, rather than as a write from a render path.
    pub fn sql_buffers(&self, id: ConnectionId) -> Vec<SqlBuffer> {
        match self.tab_ui.read().get(&id) {
            Some(ui) => ui.sql.list().to_vec(),
            None => SqlBuffers::default().list().to_vec(),
        }
    }

    /// Which buffer one tab's SQL pane is showing.
    pub fn active_sql_buffer(&self, id: ConnectionId) -> u64 {
        self.tab_ui
            .read()
            .get(&id)
            .map_or(FIRST_SQL_BUFFER, |ui| ui.sql.active())
    }

    /// What the SQL pane should currently be displaying: the active buffer
    /// and the generation of the last text placed into a buffer from outside
    /// the editor (see [`SqlBuffers::doc_target`]). The pane pushes the
    /// document into CodeMirror whenever this changes — the buffer id alone
    /// would miss an open that reuses the buffer already on screen.
    pub fn sql_doc_target(&self, id: ConnectionId) -> (u64, u64) {
        self.tab_ui
            .read()
            .get(&id)
            .map_or((FIRST_SQL_BUFFER, 0), |ui| ui.sql.doc_target())
    }

    /// One buffer's text, empty when it is gone.
    pub fn sql_buffer_text(&self, id: ConnectionId, buffer: u64) -> String {
        self.tab_ui
            .read()
            .get(&id)
            .map_or(String::new(), |ui| ui.sql.text(buffer).to_string())
    }

    /// Stores one buffer's editor text (synced from the webview on change).
    /// An actual text change invalidates that buffer's pending write
    /// confirmation — the banner must never run SQL that no longer matches
    /// the buffer.
    pub fn set_sql_text(mut self, id: ConnectionId, buffer: u64, text: String) {
        let changed = self
            .tab_ui
            .write()
            .entry(id)
            .or_default()
            .sql
            .set_text(buffer, text);
        if changed {
            self.pending_sql.write().remove(&(id, buffer));
        }
    }

    /// Shows `text` in the SQL pane without losing what is already being
    /// written (FRE-113): a new buffer, unless the active one is an untitled
    /// blank. Returns the buffer now active.
    pub fn open_sql_buffer(mut self, id: ConnectionId, title: Option<String>, text: String) -> u64 {
        self.tab_ui
            .write()
            .entry(id)
            .or_default()
            .sql
            .open(title, text)
    }

    /// Adds an empty scratch buffer and switches to it.
    pub fn new_sql_buffer(self, id: ConnectionId) -> u64 {
        self.open_sql_buffer(id, None, String::new())
    }

    /// Switches the SQL pane to another buffer.
    pub fn select_sql_buffer(mut self, id: ConnectionId, buffer: u64) {
        self.tab_ui
            .write()
            .entry(id)
            .or_default()
            .sql
            .select(buffer);
    }

    /// Closes one buffer, **cancelling its in-flight run** and dropping the
    /// run, the pending confirmation and the stale-run bookkeeping that
    /// belonged to it.
    ///
    /// Cancelling is the whole point, not tidiness: closing the tab takes its
    /// Cancel button with it, so a run left alive would keep burning CPU and
    /// pinning a pool connection with nothing anywhere able to stop it, and
    /// its results are discarded on arrival regardless. This is what
    /// [`Self::close_connection`] does for a whole tab; a query tab is the
    /// same situation one buffer down. (As there, cancelling only drops the
    /// future — the statement already in flight still finishes server-side;
    /// see [`Self::cancel_sql`].)
    pub fn close_sql_buffer(mut self, id: ConnectionId, buffer: u64) {
        self.tab_ui.write().entry(id).or_default().sql.close(buffer);
        let task = self.sql_tasks.write().remove(&(id, buffer));
        if let Some(task) = task {
            task.cancel();
        }
        self.sql_runs.write().remove(&(id, buffer));
        self.pending_sql.write().remove(&(id, buffer));
        // Removing the generation makes any still-alive task for this buffer
        // stale, so a completing run can't resurrect the closed tab's entry.
        self.sql_generations.write().remove(&(id, buffer));
    }

    /// Runs a free-form SQL script against one connection. Scripts where
    /// any statement can mutate the database (see [`needs_confirmation`])
    /// are not executed yet: they are stashed in [`Self::pending_sql`] and
    /// the editor shows a confirmation banner.
    ///
    /// A statement the connection's capabilities forbid (FRE-87) is refused
    /// here, *before* the confirmation banner — being asked to confirm a
    /// write that can never run is a prompt with no right answer.
    pub fn run_sql(mut self, id: ConnectionId, buffer: u64, sql: String) {
        self.pending_sql.write().remove(&(id, buffer));
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
            self.claim_run_slot(id, buffer);
            self.sql_runs.write().insert(
                (id, buffer),
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
                (id, buffer),
                PendingSql {
                    script: sql,
                    statements,
                },
            );
            return;
        }
        self.execute_script(id, buffer, sql, statements);
    }

    /// Confirms the write banner: runs the stashed script.
    pub fn confirm_pending_sql(mut self, id: ConnectionId, buffer: u64) {
        let pending = self.pending_sql.write().remove(&(id, buffer));
        if let Some(pending) = pending {
            self.execute_script(id, buffer, pending.script, pending.statements);
        }
    }

    /// Dismisses the write banner without running anything.
    pub fn dismiss_pending_sql(mut self, id: ConnectionId, buffer: u64) {
        self.pending_sql.write().remove(&(id, buffer));
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
    pub fn cancel_sql(mut self, id: ConnectionId, buffer: u64) {
        let task = self.sql_tasks.write().remove(&(id, buffer));
        let Some(task) = task else { return };
        task.cancel();
        if let Some(run) = self.sql_runs.write().get_mut(&(id, buffer)) {
            if run.status == RunStatus::Running {
                run.status = RunStatus::Cancelled;
            }
        }
    }

    /// Takes ownership of one query tab's SQL run slot: cancels any
    /// still-running task **of that tab** and bumps its generation, so a run
    /// that completes later can tell it has been superseded and leaves the
    /// new result alone. Returns the new generation.
    ///
    /// Per buffer, so re-running in one query tab no longer cancels or
    /// discards a run another tab has going.
    fn claim_run_slot(&mut self, id: ConnectionId, buffer: u64) -> u64 {
        let previous = self.sql_tasks.write().remove(&(id, buffer));
        if let Some(previous) = previous {
            previous.cancel();
        }
        let mut generations = self.sql_generations.write();
        let entry = generations.entry((id, buffer)).or_insert(0);
        *entry += 1;
        *entry
    }

    /// Executes a split script in the background: reads fetch rows, writes
    /// report affected counts, execution stops at the first error. Each
    /// statement's outcome lands in [`Self::sql_runs`] as it finishes.
    fn execute_script(
        mut self,
        id: ConnectionId,
        buffer: u64,
        script: String,
        statements: Vec<String>,
    ) {
        let Some((pool, caps)) = self
            .registry
            .read()
            .get(id)
            .map(|c| (c.pool.clone(), c.capabilities()))
        else {
            return;
        };
        let generation = self.claim_run_slot(id, buffer);
        self.sql_runs.write().insert(
            (id, buffer),
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
                if self.sql_generation(id, buffer) == generation {
                    if let Some(run) = self.sql_runs.write().get_mut(&(id, buffer)) {
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
            if self.sql_generation(id, buffer) != generation {
                return;
            }
            self.sql_tasks.write().remove(&(id, buffer));
            if let Some(run) = self.sql_runs.write().get_mut(&(id, buffer)) {
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
        self.sql_tasks.write().insert((id, buffer), task);
    }

    fn sql_generation(self, id: ConnectionId, buffer: u64) -> u64 {
        self.sql_generations
            .read()
            .get(&(id, buffer))
            .copied()
            .unwrap_or(0)
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
        let Some(locator) = self.connection_locator(id) else {
            return;
        };
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
        let Some(locator) = self.connection_locator(id) else {
            return;
        };
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

    /// The locator one open connection was opened from — the scope a saved
    /// query or a history entry belongs to. `None` once the tab is gone.
    pub fn connection_locator(&self, id: ConnectionId) -> Option<String> {
        self.open_locators
            .read()
            .iter()
            .find(|(open_id, _)| *open_id == id)
            .map(|(_, locator)| locator.clone())
    }

    /// Saves editor text under a name (FRE-113), scoped to this connection or
    /// global. Best-effort and off the UI path like the history writes; the
    /// outcome — including a failure — lands in [`Self::saved_status`], and a
    /// success bumps [`Self::saved_nonce`] so open panels re-query.
    ///
    /// A successful save also titles the buffer it came from, so the query
    /// tab stops reading "Query 2" the moment it has a name.
    pub fn save_query(
        mut self,
        id: ConnectionId,
        buffer: u64,
        name: String,
        description: Option<String>,
        sql: String,
        global: bool,
    ) {
        let locator = self.connection_locator(id);
        let store = self.history.read().clone();
        let Some(store) = store else {
            self.set_saved_status(
                id,
                SavedStatus::Failed("the saved-query store is unavailable".to_string()),
            );
            return;
        };
        // A connection-scoped save needs a scope; without a locator the tab
        // is already gone, and silently writing a global would put the query
        // somewhere the user did not ask for.
        if !global && locator.is_none() {
            self.set_saved_status(
                id,
                SavedStatus::Failed("this connection is no longer open".to_string()),
            );
            return;
        }
        let scope = (!global).then_some(locator).flatten();
        spawn_forever(async move {
            let outcome = store
                .save_query(&name, description.as_deref(), &sql, scope.as_deref())
                .await;
            match outcome {
                Ok(outcome) => {
                    self.set_saved_status(
                        id,
                        SavedStatus::Saved {
                            name: name.trim().to_string(),
                            replaced: outcome == SaveOutcome::Replaced,
                        },
                    );
                    self.title_sql_buffer(id, buffer, name.trim().to_string());
                    let mut nonce = self.saved_nonce.write();
                    *nonce += 1;
                }
                Err(err) => self.set_saved_status(id, SavedStatus::Failed(err)),
            }
        });
    }

    /// Records the outcome of a saved-query write for one connection's panel.
    fn set_saved_status(mut self, id: ConnectionId, status: SavedStatus) {
        self.saved_status.write().insert(id, status);
    }

    /// Names an existing buffer after the query just saved from it.
    fn title_sql_buffer(mut self, id: ConnectionId, buffer: u64, title: String) {
        let mut tab_ui = self.tab_ui.write();
        if let Some(ui) = tab_ui.get_mut(&id) {
            ui.sql.set_title(buffer, title);
        }
    }

    /// Deletes one saved query and refreshes open panels.
    pub fn delete_saved_query(mut self, id: ConnectionId, entry: i64) {
        let store = self.history.read().clone();
        let Some(store) = store else { return };
        spawn_forever(async move {
            match store.delete_saved_query(entry).await {
                Ok(()) => {
                    let mut nonce = self.saved_nonce.write();
                    *nonce += 1;
                }
                Err(err) => self.set_saved_status(id, SavedStatus::Failed(err)),
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
