//! Running SQL and getting results out: the editor buffers, the script run
//! with its write-confirmation gate and cancellation, query-history
//! recording, saved queries, and the CSV/JSON exports.
//!
//! Split out of [`super`] for the same reason as [`super::connect`]: it is
//! procedural orchestration over [`AppState`]'s signals — spawn a task, track
//! a generation, land the outcome — not state definition.

use super::*;

impl AppState {
    /// One tab's SQL buffers in tab order. A tab that has never
    /// been to its SQL pane has no entry yet; it reads as the one scratch
    /// buffer such a pane shows, rather than as a write from a render path.
    pub fn sql_buffers(&self, id: ConnectionId) -> Vec<SqlBuffer> {
        match self.tab_ui.read().get(&id) {
            Some(ui) => ui.sql.list().to_vec(),
            None => SqlBuffers::default().list().to_vec(),
        }
    }

    // There is deliberately no `active_sql_buffer`. Its only caller was the
    // editor's message handler, asking which buffer was active in order to
    // file text the editor had just sent — and those are now
    // different buffers exactly when it matters, because the editor flushes on
    // the way *out* of one. Every message now names its own buffer. Leaving
    // the accessor behind would leave the wrong answer one call away.

    /// What the SQL pane should currently be displaying: the active buffer
    /// and that buffer's document generation (see [`SqlBuffers::doc_target`]).
    /// The pane pushes the document into CodeMirror whenever this changes —
    /// the buffer id alone would miss an open that reuses the buffer already
    /// on screen — and hands the pair to the editor as the token every reply
    /// comes back stamped with.
    pub fn sql_doc_target(&self, id: ConnectionId) -> (u64, u64) {
        self.tab_ui
            .read()
            .get(&id)
            .map_or((FIRST_SQL_BUFFER, 0), |ui| ui.sql.doc_target())
    }

    /// The active buffer, its document generation, and its text, read
    /// **without subscribing**.
    ///
    /// For the two paths that hand CodeMirror a whole document: the mount and
    /// the post-wait redo. Both run inside effects that must not re-run on a
    /// keystroke, while [`Self::sql_doc_target`] reads the signal. Keeping the
    /// peek in one place is what stops either call site being rewritten as a
    /// `read` later — which would recreate the editor on every keystroke.
    pub fn peek_sql_document(&self, id: ConnectionId) -> (u64, u64, String) {
        let tab_ui = self.tab_ui.peek();
        match tab_ui.get(&id) {
            Some(ui) => {
                let (buffer, generation) = ui.sql.doc_target();
                (buffer, generation, ui.sql.text(buffer).to_string())
            }
            None => (FIRST_SQL_BUFFER, 0, String::new()),
        }
    }

    /// One buffer's text, empty when it is gone.
    pub fn sql_buffer_text(&self, id: ConnectionId, buffer: u64) -> String {
        self.tab_ui
            .read()
            .get(&id)
            .map_or(String::new(), |ui| ui.sql.text(buffer).to_string())
    }

    /// Stores one buffer's editor text, as the webview reported it.
    ///
    /// `generation` is the document generation the editor was holding when the
    /// text was typed; a message stamped with an older one describes a
    /// document that has since been replaced and is dropped (see
    /// [`SqlBuffers::set_text`]). An actual text change invalidates that
    /// buffer's pending write confirmation — the banner must never run SQL
    /// that no longer matches the buffer.
    pub fn set_sql_text(mut self, id: ConnectionId, buffer: u64, generation: u64, text: String) {
        let changed = self
            .tab_ui
            .write()
            .entry(id)
            .or_default()
            .sql
            .set_text(buffer, generation, text);
        if changed {
            self.pending_sql.write().remove(&(id, buffer));
        }
    }

    /// Replaces one buffer's text from outside the editor — the history
    /// panel's Load and Run.
    ///
    /// Moves that buffer's document generation, which does two things at once:
    /// the SQL pane pushes the new text into CodeMirror (so the call site
    /// needs no `setDoc` of its own), and the editor's in-flight report of the
    /// text being replaced is discarded instead of undoing the load.
    ///
    /// Invalidates the pending write confirmation unconditionally: unlike a
    /// keystroke this is a replacement, and a banner parked against the old
    /// text must not survive it even in the (harmless-looking) case where the
    /// loaded text happens to be identical.
    pub fn load_sql_text(mut self, id: ConnectionId, buffer: u64, text: String) {
        let loaded = self
            .tab_ui
            .write()
            .entry(id)
            .or_default()
            .sql
            .load(buffer, text);
        if loaded {
            self.pending_sql.write().remove(&(id, buffer));
        }
    }

    /// Shows `text` in the SQL pane without losing what is already being
    /// written: a new buffer, unless the active one is an untitled
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
        // Closing the key makes any still-alive task for this buffer stale,
        // so a completing run can't resurrect the closed tab's entry.
        let tasks = self.sql_runs.write().close(|key| *key == (id, buffer));
        for task in tasks {
            task.cancel();
        }
        self.pending_sql.write().remove(&(id, buffer));
    }

    /// Runs a free-form SQL script against one connection. Scripts where
    /// any statement can mutate the database (see [`needs_confirmation`])
    /// are not executed yet: they are stashed in [`Self::pending_sql`] and
    /// the editor shows a confirmation banner.
    ///
    /// A statement the connection's capabilities forbid is refused
    /// here, *before* the confirmation banner — being asked to confirm a
    /// write that can never run is a prompt with no right answer.
    pub fn run_sql(self, id: ConnectionId, buffer: u64, sql: String) {
        let statements = match self.registry.read().get(id) {
            Some(connection) => split_statements(&sql, connection.pool.dialect()),
            None => return, // connection closed underneath the editor
        };
        self.start_sql(id, buffer, sql, statements, false);
    }

    /// Explains one query tab's buffer: every statement in it gets
    /// its connection's `EXPLAIN` prefix, and the result renders as a query
    /// plan instead of a grid.
    ///
    /// **Runs through [`Self::start_sql`] like any other script**, which is the
    /// whole of the safety design. `EXPLAIN ANALYZE` executes what it explains,
    /// so a plan view with an execution path of its own would be a second place
    /// to keep the write-confirmation and capability gates correct — and the
    /// first one to fall behind. [`explain_statement`] only ever *adds* a
    /// prefix (never `ANALYZE`), and `script`'s `explaining_never_loosens_the_gate`
    /// pins that such a prefix can never lower what a statement is charged for.
    ///
    /// Explains the buffer, not a selection: Ctrl+Enter reads the selection
    /// from CodeMirror as it fires, and a button press has no such event to
    /// read it from. It therefore also uses the buffer text as last synced
    /// from the editor, which the bundle sends on a 250 ms trailing timer —
    /// the same small staleness the saved-query and history paths already
    /// live with. Pressing the button blurs the editor, which flushes that
    /// timer early, but the flush and the click reach Rust over
    /// different channels, so this is narrowed rather than closed.
    pub fn run_explain(self, id: ConnectionId, buffer: u64) {
        let backend = self.registry.read().get(id).and_then(|connection| {
            Some((
                connection.pool.dialect(),
                connection.pool.explain_support()?,
            ))
        });
        // No EXPLAIN on this backend (or the tab closed underneath the
        // button): nothing to run, and the disabled button already says why.
        let Some((dialect, support)) = backend else {
            return;
        };
        let statements: Vec<String> = split_statements(&self.sql_buffer_text(id, buffer), dialect)
            .iter()
            .map(|statement| explain_statement(statement, support))
            .collect();
        // What history records and the banner counts: the statements that
        // will actually run, not the text they were built from.
        let script = statements.join(";\n");
        self.start_sql(id, buffer, script, statements, true);
    }

    /// The shared entry point behind [`Self::run_sql`] and
    /// [`Self::run_explain`]: one capability gate, one confirmation gate, one
    /// execution path.
    fn start_sql(
        mut self,
        id: ConnectionId,
        buffer: u64,
        sql: String,
        statements: Vec<String>,
        explain: bool,
    ) {
        self.pending_sql.write().remove(&(id, buffer));
        // Effective capabilities: the backend's, narrowed by the user's
        // marking. Reading `pool.backend_capabilities()` here instead would
        // let a script write to a connection marked read-only.
        let (dialect, caps) = match self.registry.read().get(id) {
            Some(connection) => (connection.pool.dialect(), connection.capabilities()),
            None => return, // connection closed underneath the editor
        };
        if statements.is_empty() {
            return;
        }
        if let Some((statement_index, reason)) = script_refusal(caps, &statements, dialect) {
            // Claim the run slot like a real run does: an in-flight script
            // must not push its results into the refusal or overwrite it.
            self.claim_run_slot(
                id,
                buffer,
                SqlRun {
                    statements: Vec::new(),
                    status: RunStatus::Refused {
                        reason: reason.to_string(),
                        statement_index,
                        preview: statement_preview(&statements[statement_index]),
                    },
                    explain,
                    late_rest: None,
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
                    explain,
                },
            );
            return;
        }
        self.execute_script(id, buffer, sql, statements, explain);
    }

    /// Confirms the write banner: runs the stashed script.
    pub fn confirm_pending_sql(mut self, id: ConnectionId, buffer: u64) {
        let pending = self.pending_sql.write().remove(&(id, buffer));
        if let Some(pending) = pending {
            self.execute_script(
                id,
                buffer,
                pending.script,
                pending.statements,
                pending.explain,
            );
        }
    }

    /// Dismisses the write banner without running anything.
    pub fn dismiss_pending_sql(mut self, id: ConnectionId, buffer: u64) {
        self.pending_sql.write().remove(&(id, buffer));
    }

    /// Aborts the in-flight run, keeping the outcomes of the statements
    /// that already finished visible and marking the run cancelled.
    ///
    /// Cancelling only drops the run's future — on every backend the
    /// in-flight statement itself is NOT interrupted and may still complete
    /// server-side. Its connection is closed on every path, as it is when a
    /// run ends — a run's connection never goes back to the pool. What that
    /// means for the statement depends on the path:
    /// - In hubro's own transaction (the atomic path), closing ends the
    ///   transaction, so once the statement finishes nothing of the run is
    ///   kept — unless a SQL Server batch had committed that transaction
    ///   itself, whose committed part stands.
    /// - Outside it (the sequential path, which every one-statement script
    ///   takes), the script may have a transaction of its own open. A statement
    ///   that commits on its own (a cancelled autocommit UPDATE) may still
    ///   take effect; whatever the script left uncommitted is discarded by the
    ///   server. SQLite's worker thread runs the statement to completion
    ///   before the close, and keeps the write lock until then: an edit made
    ///   meanwhile waits SQLite's 5 s busy timeout and fails with "database is
    ///   locked" (measured the same on the atomic and sequential paths, before
    ///   and after runs closed their connections). Postgres and SQL Server
    ///   notice the disconnect when they next send to the client.
    /// - On a connection marked read-only, Postgres's per-statement
    ///   transaction ends with the connection, as its rollback would have
    ///   ended it; SQLite's read-only connection is closed.
    ///
    /// Either way each cancelled long-running statement keeps its server
    /// session busy until it finishes.
    pub fn cancel_sql(mut self, id: ConnectionId, buffer: u64) {
        let task = self.sql_runs.write().take_task(&(id, buffer));
        let Some(task) = task else { return };
        task.cancel();
        if let Some(run) = self.sql_runs.write().status_mut(&(id, buffer)) {
            if run.status == RunStatus::Running {
                run.status = RunStatus::Cancelled;
            }
        }
    }

    /// Takes ownership of one query tab's SQL run slot with `run`: cancels
    /// any still-running task **of that tab** and bumps its generation, so a
    /// run that completes later can tell it has been superseded and leaves
    /// the new result alone. Returns the new generation.
    ///
    /// Per buffer, so re-running in one query tab no longer cancels or
    /// discards a run another tab has going.
    fn claim_run_slot(&mut self, id: ConnectionId, buffer: u64, run: SqlRun) -> u64 {
        // A new run replaces the results on screen, so a finished export of
        // one of them has been reported already and its line goes with them
        // (see `run_retires_sql_export`).
        let source = self
            .tab_peek(id, |tab| tab.sql_export_source.clone())
            .flatten();
        if run_retires_sql_export(&self.export_status.peek(), source.as_ref(), id, buffer) {
            self.tab_mut(id, |tab| tab.sql_export_source = None);
        }
        let (generation, previous) = self.sql_runs.write().begin((id, buffer), run);
        if let Some(previous) = previous {
            previous.cancel();
        }
        generation
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
        explain: bool,
    ) {
        let Some((pool, caps)) = self
            .registry
            .read()
            .get(id)
            .map(|c| (c.pool.clone(), c.capabilities()))
        else {
            return;
        };
        let generation = self.claim_run_slot(
            id,
            buffer,
            SqlRun {
                statements: Vec::new(),
                status: RunStatus::Running,
                explain,
                late_rest: None,
            },
        );
        // spawn_forever: the run must survive pane/tab switches unmounting
        // the editor component. No signal borrow is held across an await —
        // the pool is cloned out above and every write below is scoped.
        let task = spawn_forever(async move {
            let started = std::time::Instant::now();
            let (result, pending) =
                run_script_watching_rest(&pool, caps, &statements, |statement| {
                    // Peeked first, so a stale run's progress wakes no reader.
                    if self.sql_runs.peek().is_current(&(id, buffer), generation) {
                        if let Some(run) =
                            self.sql_runs.write().current_mut(&(id, buffer), generation)
                        {
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
            // status update below.
            let (status, success, error_text) = finished_run(result, elapsed_ms);
            spawn_forever(async move {
                self.record_history(id, script, success, error_text).await;
            });
            // Stale-run guard: a newer run (or a close) owns the slot now.
            if !finish_in(self.sql_runs, &(id, buffer), generation) {
                return;
            }
            if let Some(run) = self.sql_runs.write().status_mut(&(id, buffer)) {
                run.status = status;
            }
            // The last read's rest is still being read: say how it ended once
            // it has.
            if let Some(pending) = pending {
                self.watch_late_rest(id, buffer, generation, pending);
            }
        });
        self.sql_runs.write().attach((id, buffer), task);
    }

    /// Waits for the outcome of a rest read after the run, then lands it
    /// ([`Self::land_late_rest`]). A task of its own, so the run is over and
    /// its handle released; spawn_forever, so it outlives the editor.
    fn watch_late_rest(
        self,
        id: ConnectionId,
        buffer: u64,
        generation: u64,
        pending: crate::db::PendingRest,
    ) {
        spawn_forever(async move {
            let outcome = pending.outcome().await;
            self.land_late_rest(id, buffer, generation, outcome);
        });
    }

    /// Puts a late rest's outcome on the statement it belongs to, when the
    /// run it came from is still the one in its query tab
    /// ([`lands_late_rest`]).
    fn land_late_rest(
        mut self,
        id: ConnectionId,
        buffer: u64,
        generation: u64,
        outcome: crate::db::LateRest,
    ) {
        let current = self.sql_runs.peek().is_current(&(id, buffer), generation);
        // The gate `tab_mut` uses: a closed tab's connection has left the
        // registry (its `TabState` is made lazily, so its absence says nothing).
        let tab_open = self.registry.peek().get(id).is_some();
        let lands = self
            .sql_runs
            .peek()
            .get(&(id, buffer))
            .is_some_and(|run| lands_late_rest(current, tab_open, &run.status));
        if !lands {
            return;
        }
        if let Some(run) = self.sql_runs.write().status_mut(&(id, buffer)) {
            let unconfirmed = run
                .statements
                .iter()
                .rposition(|s| s.rest == crate::db::RestOfRead::Unconfirmed);
            if let Some(index) = unconfirmed {
                run.late_rest = Some((index, outcome));
            }
        }
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

    /// Saves editor text under a name, scoped to this connection or global
    /// ([`SaveScope`]). Best-effort and off the UI path like the history
    /// writes; the outcome — including a failure — lands in
    /// [`Self::saved_status`], and a success bumps [`Self::saved_nonce`] so
    /// open panels re-query.
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
        scope: SaveScope,
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
        if scope == SaveScope::Connection && locator.is_none() {
            self.set_saved_status(
                id,
                SavedStatus::Failed("this connection is no longer open".to_string()),
            );
            return;
        }
        let scope = match scope {
            SaveScope::Connection => locator,
            SaveScope::Global => None,
        };
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
    fn set_saved_status(self, id: ConnectionId, status: SavedStatus) {
        self.tab_mut(id, |tab| tab.saved_status = Some(status));
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
    /// [`Self::export_status`].
    ///
    /// The export runs on a thread of its own ([`off_the_ui_thread`]), driven
    /// there with `block_on`: formatting each row and writing the file are
    /// synchronous, and inside the UI task they ran on the thread that draws
    /// the window, between the stream's awaits.
    pub fn export_query(
        mut self,
        id: ConnectionId,
        table: TableRef,
        sql: String,
        params: Vec<Value>,
        format: ExportFormat,
        path: PathBuf,
    ) {
        let slot = (id, ExportPane::Grid);
        self.tab_mut(id, |tab| tab.grid_export_table = Some(table));
        // With its `Reading`, which decides whether the export runs where the
        // engine refuses writes — see `DbPool::export`.
        let pool = self.registry.read().get(id).map(|c| c.browse());
        let Some((pool, reading)) = pool else {
            let _ = self
                .export_status
                .write()
                .begin(slot, ExportStatus::Failed("connection closed".into()));
            return;
        };
        let generation = self.begin_export(slot);
        // spawn_forever: the export must survive the grid unmounting (a pane
        // or tab switch) — a plain spawn would cancel it mid-write.
        spawn_forever(async move {
            let runtime = tokio::runtime::Handle::current();
            // Stream into a temp file and rename on success, so a mid-stream
            // failure can't clobber an existing file at `path`.
            let tmp = export_temp_path(&path);
            let outcome = off_the_ui_thread({
                let tmp = tmp.clone();
                move || {
                    use std::io::Write as _;
                    runtime.block_on(async {
                        let file = std::fs::File::create(&tmp).map_err(|e| e.to_string())?;
                        let mut writer = std::io::BufWriter::new(file);
                        let rows = pool
                            .export(reading, &sql, &params, format, &mut writer)
                            .await
                            .map_err(|e| e.to_string())?;
                        writer.flush().map_err(|e| e.to_string())?;
                        drop(writer);
                        std::fs::rename(&tmp, &path).map_err(|e| e.to_string())?;
                        Ok::<u64, String>(rows)
                    })
                }
            })
            .await;
            // Out here rather than in the closure, so a panic in the export
            // (reported as a failed join) does not leave the temp file behind.
            if outcome.is_err() {
                let _ = std::fs::remove_file(&tmp);
            }
            self.finish_export(slot, generation, outcome);
        });
    }

    /// Writes one of the SQL editor's held results to `path` in `format`, on
    /// a thread of its own ([`off_the_ui_thread`]). Shares the row formatters
    /// with [`Self::export_query`]; no database round-trip.
    ///
    /// Takes the shared statement rather than its rows: the export borrows
    /// them through the `Arc` instead of copying every row on the UI thread
    /// first, and the statement is recorded as the export's source in
    /// [`Self::sql_export_sources`] with the query tab it came from, so the
    /// status line lands on that result alone ([`sql_export_status`]) — or
    /// under that tab's run, if the result has been replaced by the time the
    /// export starts ([`orphaned_sql_export_status`]).
    pub fn export_result(
        self,
        id: ConnectionId,
        buffer: u64,
        statement: SharedStatement,
        format: ExportFormat,
        path: PathBuf,
    ) {
        let slot = (id, ExportPane::Sql);
        self.tab_mut(id, |tab| {
            tab.sql_export_source = Some(SqlExportSource {
                buffer,
                statement: statement.downgrade(),
            })
        });
        let generation = self.begin_export(slot);
        spawn_forever(async move {
            let tmp = export_temp_path(&path);
            let outcome = off_the_ui_thread({
                let tmp = tmp.clone();
                move || {
                    use std::io::Write as _;
                    let StatementOutcome::Rows(result) = &statement.outcome else {
                        return Err("this statement returned no rows".to_string());
                    };
                    (|| {
                        let file = std::fs::File::create(&tmp).map_err(|e| e.to_string())?;
                        let mut writer = std::io::BufWriter::new(file);
                        let rows =
                            write_result(result, format, &mut writer).map_err(|e| e.to_string())?;
                        writer.flush().map_err(|e| e.to_string())?;
                        drop(writer);
                        std::fs::rename(&tmp, &path).map_err(|e| e.to_string())?;
                        Ok::<u64, String>(rows)
                    })()
                }
            })
            .await;
            // Out here so a panic in the write cleans up too (see above).
            if outcome.is_err() {
                let _ = std::fs::remove_file(&tmp);
            }
            self.finish_export(slot, generation, outcome);
        });
    }

    /// Clears a finished grid export's line — its Dismiss button. A running
    /// export offers none, so its outcome cannot be dismissed before it is
    /// known.
    pub fn clear_grid_export_status(mut self, id: ConnectionId) {
        self.export_status.write().clear(&(id, ExportPane::Grid));
        self.tab_mut(id, |tab| tab.grid_export_table = None);
    }

    /// Marks a slot Running and returns the new export's generation.
    fn begin_export(mut self, slot: (ConnectionId, ExportPane)) -> u64 {
        let (generation, _no_task) = self
            .export_status
            .write()
            .begin(slot, ExportStatus::Running);
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
        if !finish_in(self.export_status, &slot, generation) {
            return;
        }
        let status = match outcome {
            Ok(rows) => ExportStatus::Done { rows },
            Err(err) => ExportStatus::Failed(err),
        };
        self.export_status.write().set(slot, status);
    }
}

/// Runs an export's synchronous work — formatting rows, writing and renaming
/// the file — on tokio's blocking pool, and waits for it there.
///
/// Dioxus desktop polls its tasks on the thread that draws the window, so a
/// task that does synchronous work holds the window for as long as the work
/// takes; an export of a large result froze it. The blocking pool is the same
/// place [`crate::secrets`] sends keyring calls for the same reason.
async fn off_the_ui_thread<T: Send + 'static>(
    work: impl FnOnce() -> Result<T, String> + Send + 'static,
) -> Result<T, String> {
    tokio::task::spawn_blocking(work)
        .await
        .unwrap_or_else(|join_err| Err(format!("export task failed: {join_err}")))
}

/// Sibling temp path for an atomic export write (`foo.csv` → `foo.csv.part`).
/// Streaming into this and renaming on success keeps a mid-stream failure
/// from clobbering an existing file at the destination.
/// What a finished run shows and what history records of it.
///
/// History records whether the run's work took effect as it was written: a
/// run whose transaction hubro rolled back is recorded as failed, with the
/// reason, even though every statement ran — "success" beside a script whose
/// changes are gone would send the user looking for rows that are not there.
/// A run whose connection was closed after it is recorded as a success: every
/// statement ran, and nothing is known to be lost.
fn finished_run(
    result: Result<ScriptDone, ScriptError>,
    elapsed_ms: u64,
) -> (RunStatus, bool, Option<String>) {
    match result {
        Ok(done) => {
            let rolled_back = done.rolled_back.map(|_| {
                "The script ended inside a transaction it never committed; hubro rolled it back."
                    .to_string()
            });
            (
                RunStatus::Done {
                    elapsed_ms,
                    rolled_back: done.rolled_back,
                    connection_closed: done.connection_closed,
                },
                rolled_back.is_none(),
                rolled_back,
            )
        }
        Err(err) => {
            let error = err.error.to_string();
            (
                RunStatus::Failed {
                    error: error.clone(),
                    statement_index: err.statement_index,
                    preview: err.preview,
                    elapsed_ms,
                    rollback: err.rollback,
                },
                false,
                Some(error),
            )
        }
    }
}

/// Whether a late rest's outcome may land on the run in a query tab: only
/// on the run it came from (`current`: no newer run has begun there, and the
/// slot was not closed with its tab), while the connection's tab is open, and
/// only on a run that finished — a cancelled or failed run left no
/// unconfirmed rest to resolve. Over plain data so a test can execute it.
fn lands_late_rest(current: bool, tab_open: bool, status: &RunStatus) -> bool {
    current && tab_open && matches!(status, RunStatus::Done { .. })
}

fn export_temp_path(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".part");
    path.with_file_name(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn export_work_runs_on_a_thread_other_than_the_callers() {
        // The caller stands in for the UI task: Dioxus desktop polls tasks on
        // the thread that draws the window, so work done on the caller's
        // thread holds the window for as long as it takes.
        let caller = std::thread::current().id();
        let worker = off_the_ui_thread(|| Ok(std::thread::current().id()))
            .await
            .unwrap();
        assert_ne!(caller, worker);
    }

    #[tokio::test]
    async fn a_panicking_export_reports_a_failure() {
        // The callers remove the temp file on any error, so a panic must come
        // back as one rather than unwind past them.
        let outcome = off_the_ui_thread(|| -> Result<(), String> { panic!("boom") }).await;
        assert!(
            matches!(&outcome, Err(e) if e.starts_with("export task failed")),
            "{outcome:?}"
        );
    }

    /// A [`VirtualDom`] with an [`AppState`] holding one in-memory SQLite
    /// connection.
    fn state_with_connection() -> (dioxus::core::VirtualDom, AppState, ConnectionId) {
        use crate::db::WriteProtection;
        let dom = dioxus::core::VirtualDom::new(|| rsx! {});
        let state = crate::ui::test_dom::provide_state(&dom);
        let id = dom.in_runtime(|| {
            let pool =
                DbPool::Sqlite(sqlx::sqlite::SqlitePool::connect_lazy("sqlite::memory:").unwrap());
            let mut registry = state.registry;
            let id = registry.write().insert("t.db", pool, WriteProtection::Open);
            id
        });
        (dom, state, id)
    }

    /// The real grid export records the table it was taken from, so its
    /// line names that table once another is on screen — rather than putting
    /// "Exported 1 row" under a table it says nothing about.
    #[tokio::test]
    async fn a_grid_export_names_its_table_under_another_one() {
        let (mut dom, state, id) = state_with_connection();
        let albums = TableRef {
            schema: None,
            name: "albums".into(),
        };
        let tracks = TableRef {
            schema: None,
            name: "tracks".into(),
        };
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("albums.csv");
        dom.in_runtime(|| {
            state.export_query(
                id,
                albums.clone(),
                "SELECT 1 AS a".into(),
                Vec::new(),
                ExportFormat::Csv,
                path.clone(),
            )
        });
        let done = Some(ExportStatus::Done { rows: 1 });
        let finished = tokio::time::timeout(std::time::Duration::from_secs(10), async {
            while dom.in_runtime(|| {
                state
                    .export_status
                    .peek()
                    .get(&(id, ExportPane::Grid))
                    .cloned()
            }) != done
            {
                let _ =
                    tokio::time::timeout(std::time::Duration::from_millis(50), dom.wait_for_work())
                        .await;
            }
        })
        .await;
        assert!(finished.is_ok(), "the export never finished");
        let line = dom.in_runtime(|| {
            grid_export_line(
                &state.export_status.peek(),
                state.grid_export_table(id).as_ref(),
                id,
                &tracks,
            )
        });
        assert_eq!(
            line.map(|(text, ..)| text),
            Some("albums: Exported 1 row".into())
        );
    }

    /// A run in the tab a finished SQL export came from retires its line —
    /// through the real `run_sql`, which is where a run claims its slot — and
    /// a run in another tab leaves it alone.
    #[tokio::test]
    async fn a_run_retires_a_finished_sql_export_from_its_own_tab_only() {
        let (dom, state, id) = state_with_connection();
        let statement = SharedStatement::new(StatementResult {
            preview: "SELECT 1".into(),
            outcome: StatementOutcome::Rows(crate::db::QueryResult {
                columns: Vec::new(),
                rows: vec![vec![Value::Integer(1)]],
            }),
            truncated: false,
            rest: crate::db::RestOfRead::Ran,
        });
        dom.in_runtime(|| {
            state.tab_mut(id, |tab| {
                tab.sql_export_source = Some(SqlExportSource {
                    buffer: 1,
                    statement: statement.downgrade(),
                })
            });
            let mut statuses = state.export_status;
            statuses
                .write()
                .set((id, ExportPane::Sql), ExportStatus::Done { rows: 1 });

            state.run_sql(id, 2, "SELECT 1".into());
            assert!(state.sql_export_source(id).is_some(), "another tab's run");
            state.run_sql(id, 1, "SELECT 1".into());
            assert!(state.sql_export_source(id).is_none(), "its own tab's run");
        });
    }

    use crate::db::OpenTransaction;

    /// A late outcome for a connection whose tab has closed is dropped: the
    /// connection left the registry while its run's slot still matched.
    #[tokio::test]
    async fn a_late_rest_for_a_closed_tab_is_dropped() {
        use crate::db::LateRest;
        let (dom, mut state, id) = state_with_connection();
        dom.in_runtime(|| {
            let generation = state.claim_run_slot(id, 0, late_run("SELECT f()"));
            let mut registry = state.registry;
            registry.write().remove(id);
            state.land_late_rest(id, 0, generation, LateRest::Finished);
            assert_eq!(state.sql_runs.peek()[&(id, 0)].late_rest, None);
        });
    }

    /// The run's watcher lands the outcome on its own run once it arrives —
    /// driven through the runtime with a stand-in for the background read.
    /// `execute_script` hands its watcher the run's own generation, pinned
    /// below as written.
    #[tokio::test]
    async fn a_watched_rest_resolves_its_own_run() {
        use crate::db::{LateRest, PendingRest};
        let (mut dom, state, id) = state_with_connection();
        let (sender, receiver) = tokio::sync::oneshot::channel();
        dom.in_runtime(|| {
            let mut state = state;
            let generation = state.claim_run_slot(id, 0, late_run("SELECT f()"));
            state.watch_late_rest(id, 0, generation, PendingRest(receiver));
        });
        assert_eq!(
            dom.in_runtime(|| state.sql_runs.peek()[&(id, 0)].late_rest.clone()),
            None
        );
        sender.send(LateRest::Failed("boom".into())).unwrap();
        let landed = tokio::time::timeout(std::time::Duration::from_secs(10), async {
            while dom
                .in_runtime(|| state.sql_runs.peek()[&(id, 0)].late_rest.clone())
                .is_none()
            {
                let _ =
                    tokio::time::timeout(std::time::Duration::from_millis(50), dom.wait_for_work())
                        .await;
            }
        })
        .await;
        assert!(landed.is_ok(), "the outcome never landed");
        assert_eq!(
            dom.in_runtime(|| state.sql_runs.peek()[&(id, 0)].late_rest.clone()),
            Some((0, LateRest::Failed("boom".into())))
        );
        let shipped =
            crate::source_scan::code_only(&crate::source_scan::shipped(include_str!("sql.rs")));
        assert_eq!(
            shipped.matches("self.watch_late_rest(").count(),
            1,
            "execute_script watches its rest once"
        );
        assert!(shipped.contains(
            "if let Some(pending) = pending { self.watch_late_rest(id, buffer, generation, pending); }"
        ));
    }

    /// A finished run whose one statement's rest is still being read.
    fn late_run(preview: &str) -> SqlRun {
        SqlRun {
            statements: vec![SharedStatement::new(StatementResult {
                preview: preview.into(),
                outcome: StatementOutcome::Rows(crate::db::QueryResult::default()),
                truncated: true,
                rest: crate::db::RestOfRead::Unconfirmed,
            })],
            status: RunStatus::Done {
                elapsed_ms: 1,
                rolled_back: None,
                connection_closed: false,
            },
            explain: false,
            late_rest: None,
        }
    }

    /// The gate a late rest's outcome passes: the run it came from, still
    /// current, in an open tab, and finished.
    #[test]
    fn a_late_rest_lands_only_on_the_finished_run_it_came_from() {
        let done = RunStatus::Done {
            elapsed_ms: 1,
            rolled_back: None,
            connection_closed: false,
        };
        assert!(lands_late_rest(true, true, &done));
        assert!(
            !lands_late_rest(false, true, &done),
            "a newer run owns the tab"
        );
        assert!(!lands_late_rest(true, false, &done), "the tab was closed");
        assert!(!lands_late_rest(true, true, &RunStatus::Cancelled));
        assert!(!lands_late_rest(true, true, &RunStatus::Running));
    }

    /// Executed through the state: a late outcome lands on its run's
    /// unconfirmed statement, and one from a run since replaced is dropped
    /// rather than resolving the new run's notice.
    #[tokio::test]
    async fn a_late_rest_from_a_superseded_run_is_dropped() {
        use crate::db::{LateRest, RestOfRead};
        let (dom, mut state, id) = state_with_connection();
        let unconfirmed = |preview: &str| {
            SharedStatement::new(StatementResult {
                preview: preview.into(),
                outcome: StatementOutcome::Rows(crate::db::QueryResult::default()),
                truncated: true,
                rest: RestOfRead::Unconfirmed,
            })
        };
        let finished = |statements: Vec<SharedStatement>| SqlRun {
            statements,
            status: RunStatus::Done {
                elapsed_ms: 1,
                rolled_back: None,
                connection_closed: false,
            },
            explain: false,
            late_rest: None,
        };
        dom.in_runtime(|| {
            let first = state.claim_run_slot(id, 0, finished(vec![unconfirmed("SELECT f()")]));
            state.land_late_rest(id, 0, first, LateRest::Finished);
            assert_eq!(
                state.sql_runs.peek()[&(id, 0)].late_rest,
                Some((0, LateRest::Finished))
            );
            let second = state.claim_run_slot(id, 0, finished(vec![unconfirmed("SELECT g()")]));
            state.land_late_rest(id, 0, first, LateRest::Failed("stale".into()));
            assert_eq!(
                state.sql_runs.peek()[&(id, 0)].late_rest,
                None,
                "a stale outcome landed"
            );
            state.land_late_rest(id, 0, second, LateRest::Failed("boom".into()));
            assert_eq!(
                state.sql_runs.peek()[&(id, 0)].late_rest,
                Some((0, LateRest::Failed("boom".into())))
            );
        });
    }

    /// The wire from what a run returned to what the editor shows and what
    /// history records — the rolled-back report must reach both.
    #[test]
    fn a_rolled_back_run_is_shown_and_recorded_as_rolled_back() {
        let open = OpenTransaction {
            schema_changes_kept: true,
        };
        let (status, success, error) = finished_run(
            Ok(ScriptDone {
                rolled_back: Some(open),
                connection_closed: false,
            }),
            7,
        );
        assert_eq!(
            status,
            RunStatus::Done {
                elapsed_ms: 7,
                rolled_back: Some(open),
                connection_closed: false,
            }
        );
        assert!(
            !success,
            "history must not record a rolled-back run as a success"
        );
        assert!(error.is_some_and(|e| e.contains("rolled it back")));

        let (status, success, error) = finished_run(
            Ok(ScriptDone {
                rolled_back: None,
                connection_closed: true,
            }),
            7,
        );
        assert_eq!(
            status,
            RunStatus::Done {
                elapsed_ms: 7,
                rolled_back: None,
                connection_closed: true,
            }
        );
        assert!(success);
        assert_eq!(error, None);

        let (_, success, error) = finished_run(Ok(ScriptDone::default()), 7);
        assert!(success);
        assert_eq!(error, None);

        let (status, success, error) = finished_run(
            Err(ScriptError {
                statement_index: 1,
                preview: "SELECT nope".into(),
                error: DbError::Query("boom".into()),
                rollback: Rollback::OpenTransaction(open),
            }),
            7,
        );
        assert!(matches!(
            status,
            RunStatus::Failed {
                rollback: Rollback::OpenTransaction(o),
                statement_index: 1,
                ..
            } if o == open
        ));
        assert!(!success);
        assert!(error.is_some_and(|e| e.contains("boom")));
    }
}
