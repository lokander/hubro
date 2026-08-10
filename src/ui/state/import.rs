//! Running a file import in the background (FRE-112) — the write-side twin
//! of the export task in [`sql`](super::sql).
//!
//! Same shape as an export, deliberately: the work runs in a `spawn_forever`
//! task so it survives the grid unmounting, its progress lands in a
//! per-connection status signal, and a generation counter keeps a slow import
//! from overwriting the status of one started after it.
//!
//! What differs is what happens at the end. An export leaves a file; an
//! import changes the table, so a finished import bumps the grid's refresh
//! nonce — otherwise the rows the user just imported are absent from the view
//! that is still showing the page fetched before them.

use super::*;

/// Progress of the most recent import on one connection.
///
/// Keyed per connection but not per pane, unlike [`ExportStatus`]: an import
/// is only ever started from the grid.
#[derive(Debug, Clone, PartialEq)]
pub enum ImportStatus {
    Running {
        /// The table being imported into, for the line's wording.
        table: String,
    },
    Done(ImportReport),
    Failed(String),
}

impl ImportStatus {
    /// The toolbar line for this status: display text plus a Tailwind color
    /// class, mirroring [`ExportStatus::line`].
    ///
    /// A completed import with skipped rows is deliberately *not* green: it
    /// succeeded, but rows the user expected are missing, and that has to
    /// read as something to look at rather than as a clean result.
    pub fn line(&self) -> (String, &'static str) {
        match self {
            ImportStatus::Running { table } => (
                format!("Importing into {table}…"),
                "text-slate-500 dark:text-slate-400",
            ),
            ImportStatus::Done(report) if report.skipped_rows > 0 => (
                format!(
                    "Imported {} row{}, skipped {}",
                    report.inserted_rows,
                    plural(report.inserted_rows),
                    report.skipped_rows
                ),
                "text-amber-700 dark:text-amber-400",
            ),
            ImportStatus::Done(report) => (
                format!(
                    "Imported {} row{}",
                    report.inserted_rows,
                    plural(report.inserted_rows)
                ),
                "text-emerald-700 dark:text-emerald-400",
            ),
            ImportStatus::Failed(err) => (
                format!("Import failed: {err}"),
                "text-red-600 dark:text-red-400",
            ),
        }
    }

    /// The full report of a finished import, for the dialog's result panel —
    /// which lists the skipped lines the one-line status only counts.
    pub fn report(&self) -> Option<&ImportReport> {
        match self {
            ImportStatus::Done(report) => Some(report),
            _ => None,
        }
    }
}

fn plural(count: u64) -> &'static str {
    if count == 1 {
        ""
    } else {
        "s"
    }
}

/// Everything one import needs, assembled by the dialog and handed to the
/// task in one piece.
#[derive(Debug, Clone, PartialEq)]
pub struct ImportRequest {
    pub path: PathBuf,
    pub format: SourceFormat,
    pub encoding: Encoding,
    pub table: TableMeta,
    pub options: ImportOptions,
}

impl AppState {
    /// Streams `request`'s file into its table in a background task. The
    /// outcome lands in [`Self::import_status`]; the UI never blocks.
    ///
    /// Capabilities are resolved *here*, from the live connection — so the
    /// user's read-only marking (FRE-111) is read at the moment the import
    /// starts rather than whenever the dialog happened to open — and passed
    /// to [`run_import`], which refuses on the same resolved answer.
    pub fn start_import(mut self, id: ConnectionId, request: ImportRequest) {
        let access = self
            .registry
            .read()
            .get(id)
            .map(|c| (c.pool.clone(), c.access(&request.table)));
        let Some((pool, access)) = access else {
            let generation = self.begin_import(id, &request.table.name);
            self.finish_import(id, generation, Err("connection closed".into()), None);
            return;
        };
        let generation = self.begin_import(id, &request.table.name);
        let table_key = TableRef {
            schema: request.table.schema.clone(),
            name: request.table.name.clone(),
        }
        .key();
        // spawn_forever: the import must survive the grid unmounting (a pane
        // or tab switch). A plain spawn would cancel it mid-transaction —
        // which would roll back, but silently and for no reason the user
        // asked for.
        //
        // The handle is kept so ONE thing can stop it: closing the
        // connection ([`AppState::close_connection`]), which cancels it the
        // way it already cancels a running script. Dropping the future drops
        // the open transaction with it, so the rows roll back — without this
        // a 20 000-row import survived `pool.close()` and committed every row
        // afterwards, with its outcome dropped as stale and nothing shown.
        let task = spawn_forever(async move {
            let outcome = async {
                let mut source = open_source(&request.path, request.format, request.encoding)
                    .map_err(|e| format!("opening the file failed: {e}"))?;
                run_import(
                    &pool,
                    &access,
                    &request.table,
                    &request.options,
                    source.as_mut(),
                )
                .await
                .map_err(|e| e.to_string())
            }
            .await;
            self.finish_import(id, generation, outcome, Some(table_key));
        });
        self.import_tasks.write().insert(id, task);
    }

    /// Marks the connection's import slot Running and returns its generation.
    fn begin_import(mut self, id: ConnectionId, table: &str) -> u64 {
        let generation = {
            let mut generations = self.import_generations.write();
            let entry = generations.entry(id).or_insert(0);
            *entry += 1;
            *entry
        };
        self.import_status.write().insert(
            id,
            ImportStatus::Running {
                table: table.to_string(),
            },
        );
        generation
    }

    /// Records an import's terminal status — unless a newer import owns the
    /// slot, in which case this outcome is stale and dropped.
    ///
    /// A successful import also refreshes the grid: the rows it just wrote
    /// are not in the page the view is still showing.
    fn finish_import(
        mut self,
        id: ConnectionId,
        generation: u64,
        outcome: Result<ImportReport, String>,
        table_key: Option<String>,
    ) {
        let latest = self.import_generations.read().get(&id).copied();
        if latest != Some(generation) {
            return;
        }
        self.release_import_task(id);
        let status = match outcome {
            Ok(report) => {
                if let Some(key) = table_key {
                    self.bump_grid_refresh(id, &key);
                }
                ImportStatus::Done(report)
            }
            Err(err) => ImportStatus::Failed(err),
        };
        self.import_status.write().insert(id, status);
    }

    /// Forgets a finished import's task handle — nothing to cancel once the
    /// outcome is in, and a stale handle would let a later close "cancel" a
    /// task that has already committed.
    fn release_import_task(mut self, id: ConnectionId) {
        self.import_tasks.write().remove(&id);
    }

    /// Clears the import line for one connection — the dialog's "dismiss"
    /// and what closing the dialog after a failure does.
    pub fn clear_import_status(mut self, id: ConnectionId) {
        self.import_status.write().remove(&id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::SkippedRow;

    #[test]
    fn the_status_line_says_what_happened() {
        let running = ImportStatus::Running {
            table: "people".into(),
        };
        assert!(running.line().0.contains("people"));
        assert!(running.report().is_none());

        let clean = ImportStatus::Done(ImportReport {
            inserted_rows: 1,
            skipped: vec![],
            skipped_rows: 0,
        });
        assert_eq!(clean.line().0, "Imported 1 row");
        assert!(clean.line().1.contains("emerald"));

        // A partial import must not read as a clean one: rows the user
        // expected are missing.
        let partial = ImportStatus::Done(ImportReport {
            inserted_rows: 3,
            skipped: vec![SkippedRow {
                line: 7,
                reason: "nope".into(),
            }],
            skipped_rows: 1,
        });
        assert_eq!(partial.line().0, "Imported 3 rows, skipped 1");
        assert!(partial.line().1.contains("amber"), "{}", partial.line().1);
        assert_eq!(partial.report().map(|r| r.skipped.len()), Some(1));

        let failed = ImportStatus::Failed("line 4: nope".into());
        assert!(failed.line().0.starts_with("Import failed:"));
        assert!(failed.line().1.contains("red"));
    }
}
