//! What a run leaves under the SQL editor: each statement's section with its
//! result table and export buttons, the error block a failing script shows,
//! and the script's status line.

use super::*;

/// Cap on rendered result rows (per statement). The query layer keeps up to
/// `MAX_QUERY_ROWS` (10,000) rows of a result; the DOM must not receive even
/// that many.
const MAX_RENDERED_ROWS: usize = 500;

/// The notice over a read the row cap cut short, and what became of its
/// rest. A cancel on a writable connection says what it cost: it undoes all
/// of the statement, rows before the cap included, and the rule that allowed
/// it looks only for table writes — a session setting or a notification the
/// statement made is undone without one (measured,
/// `postgres::knows_whether_written`). A statement stopped where nothing it
/// did could have lasted anyway (`RestOfRead::Stopped`) gets the plain line,
/// and one read after the run (`RestOfRead::Unconfirmed`) says its outcome
/// is not known.
fn truncation_notice(rest: RestOfRead, late: Option<&LateRest>) -> String {
    let notice = format!(
        "Result truncated — showing the first {MAX_QUERY_ROWS} rows to keep memory bounded. \
         Add a LIMIT or a WHERE clause to narrow it."
    );
    match rest {
        RestOfRead::Ran => notice,
        RestOfRead::Stopped => format!("{notice} The rest was not run."),
        RestOfRead::CancelledUnwritten => format!(
            "{notice} The rest was cancelled at the row cap, which undid everything the \
             statement did. Only a statement that had written no table rows is cancelled; \
             settings or notifications it made are undone too."
        ),
        RestOfRead::Unconfirmed => match late {
            None => format!(
                "{notice} The rest of this result is still being read after the run ended; \
                 this note will say how it ends. If the statement wrote anything and the rest \
                 fails, its writes are undone."
            ),
            Some(LateRest::Finished) => format!(
                "{notice} The rest of this result was read after the run ended and finished."
            ),
            Some(LateRest::Failed(error)) => format!(
                "{notice} The rest of this result failed after the run ended: {error}. If the \
                 statement wrote anything, its writes were undone."
            ),
            Some(LateRest::Unknown) => format!(
                "{notice} The rest of this result was read after the run ended, and the \
                 connection was lost before it reported, so its outcome is unknown. If the \
                 statement wrote anything, its writes may have been undone."
            ),
        },
    }
}

/// The results half of the SQL pane for the active buffer: the write
/// confirmation a pending script waits on, each statement's section (or its
/// plan, for an Explain), an export line orphaned by a rerun, and the
/// script's status line.
///
/// Its own component, reading the run, the pending script and the export
/// line itself: a pending script or an export finishing repaints the results
/// alone. A run's progress still re-renders [`SqlEditor`](super::SqlEditor)
/// too, which reads the run for its Cancel button; the children it passes
/// unchanged props to skip.
#[component]
pub(super) fn RunResults(
    id: ConnectionId,
    /// The buffer whose results these are.
    active: u64,
    /// The connection's dialect, which decides which pending statements need
    /// confirming. `None` once the connection is gone: see
    /// [`writes_to_confirm`].
    dialect: Option<Dialect>,
    /// Whether this connection's plans parse into a tree.
    structured_plans: bool,
) -> Element {
    let state = use_context::<AppState>();
    // Cheap despite running every render: the statements are Arc-shared
    // [`SharedStatement`]s, so this clones pointers and the small status —
    // never the result rows.
    //
    // Filtered to the active buffer: one connection still runs one script at
    // a time, but its results belong to the buffer that ran them.
    let run = state.sql_runs.read().get(&(id, active)).cloned();
    let pending_writes = state
        .pending_sql
        .read()
        .get(&(id, active))
        .and_then(|pending| writes_to_confirm(&pending.statements, dialect));
    // Under Confirm the banner names the connection: the whole
    // point of the state is to make you read *which* database you are about
    // to change, which "Run anyway?" alone never made you do.
    let confirm_target: Option<String> = state
        .confirms_writes(id)
        .then(|| state.registry.read().get(id).map(|c| c.name.clone()))
        .flatten();
    rsx! {
        div { class: "min-h-0 flex-1 overflow-auto",
            if let Some(write_count) = pending_writes {
                div { class: "flex items-center gap-3 border-b border-amber-300 dark:border-amber-900/50 bg-amber-100 dark:bg-amber-950/30 px-4 py-2",
                    span { class: "text-sm text-amber-700 dark:text-amber-300",
                        if let Some(target) = confirm_target.clone() {
                            if write_count == 1 {
                                "Run 1 write statement against \"{target}\"?"
                            } else {
                                "Run {write_count} write statements against \"{target}\"?"
                            }
                        } else if write_count == 1 {
                            "This script contains 1 write statement. Run anyway?"
                        } else {
                            "This script contains {write_count} write statements. Run anyway?"
                        }
                    }
                    button {
                        class: "rounded bg-amber-600 px-2.5 py-0.5 text-xs font-semibold text-slate-950 hover:bg-amber-500",
                        onclick: move |_| state.confirm_pending_sql(id, active),
                        "Run"
                    }
                    button {
                        class: "rounded border border-slate-400 dark:border-slate-600 px-2.5 py-0.5 text-xs text-slate-900 dark:text-slate-300 hover:bg-slate-200 dark:hover:bg-slate-800",
                        onclick: move |_| state.dismiss_pending_sql(id, active),
                        "Cancel"
                    }
                }
            }
            match run {
                None => rsx! {
                    EmptyState {
                        icon: rsx! { Play { size: 40 } },
                        title: "Results appear here",
                        hint: "Write SQL above and press Ctrl+Enter to run it.",
                    }
                },
                Some(run) => rsx! {
                    for (index, statement) in run.statements.iter().enumerate() {
                        if run.explain {
                            PlanSection {
                                key: "{index}",
                                index: index + 1,
                                structured: structured_plans,
                                result: statement.clone(),
                            }
                        } else {
                            StatementSection {
                                key: "{index}",
                                id,
                                buffer: active,
                                index: index + 1,
                                result: statement.clone(),
                                late: run
                                    .late_rest
                                    .as_ref()
                                    .filter(|(at, _)| *at == index)
                                    .map(|(_, late)| late.clone()),
                            }
                        }
                    }
                    if let Some(status) = orphaned_sql_export_status(
                        &state.export_status.read(),
                        state.sql_export_source(id).as_ref(),
                        id,
                        active,
                        &run.statements,
                    ) {
                        {
                            let (text, tone) = status.line();
                            rsx! {
                                p { class: "px-4 py-2 text-xs {tone.class()}",
                                    "Export of an earlier result: {text}"
                                }
                            }
                        }
                    }
                    RunStatusLine { status: run.status.clone(), statement_count: run.statements.len() }
                },
            }
        }
    }
}

/// How many of a pending script's statements the confirmation banner counts
/// as writes — `None`, so no banner, without a dialect to judge them by.
///
/// The count is a claim ("this script contains 2 write statements"), and
/// which statements need confirming is the dialect's call: guessing one
/// would state a number about another engine. A connection without a dialect
/// is closing, so there is nothing left to run the script against either.
pub(super) fn writes_to_confirm(statements: &[String], dialect: Option<Dialect>) -> Option<usize> {
    let dialect = dialect?;
    Some(
        statements
            .iter()
            .filter(|s| needs_confirmation(s, dialect))
            .count(),
    )
}

/// The script-level footer: running indicator, total elapsed time (shown
/// once per run), or the failure block.
#[component]
fn RunStatusLine(status: RunStatus, statement_count: usize) -> Element {
    match status {
        RunStatus::Running => rsx! {
            LoadingLine { label: "Running…" }
        },
        RunStatus::Done {
            elapsed_ms,
            rolled_back,
            connection_closed,
        } => rsx! {
            p { class: "px-4 py-2 text-xs text-slate-500",
                if statement_count == 1 {
                    "1 statement in {elapsed_ms} ms"
                } else {
                    "{statement_count} statements in {elapsed_ms} ms"
                }
            }
            // Every statement succeeded, and still none of what the script did
            // inside its transaction is in the database — the one thing a
            // timing line must not leave the user to discover.
            if let Some(open) = rolled_back {
                p { class: "border-t border-amber-300 dark:border-amber-900/50 px-4 py-3 text-sm text-amber-700 dark:text-amber-300",
                    {left_open_note(open)}
                }
            }
            if connection_closed {
                p { class: "border-t border-amber-300 dark:border-amber-900/50 px-4 py-3 text-sm text-amber-700 dark:text-amber-300",
                    "Hubro could not check whether the script left a transaction open, so it \
                     closed the connection — anything the script had not committed was discarded."
                }
            }
        },
        RunStatus::Failed {
            error,
            statement_index,
            preview,
            elapsed_ms,
            rollback,
        } => rsx! {
            StatementErrorBlock {
                statement_index,
                preview,
                message: error,
                note: match rollback {
                    Rollback::Full => {
                        format!("Script rolled back after {elapsed_ms} ms — no changes were applied.")
                    }
                    Rollback::None => format!(
                        "Script stopped after {elapsed_ms} ms; hubro rolled nothing back, so \
                         earlier statements stand unless the server undid them itself.",
                    ),
                    Rollback::ConnectionClosed => format!(
                        "Script stopped after {elapsed_ms} ms, and hubro could not check what \
                         the script left open, so the connection was closed: the server \
                         discarded anything it had not committed. Statements it committed stand.",
                    ),
                    Rollback::AlreadyEnded => format!(
                        "Script stopped after {elapsed_ms} ms, but its transaction had already \
                         ended: the script committed it, or the server rolled it back (the error \
                         says which). Hubro rolled nothing back, so what ran before it ended — \
                         part of the failing statement included — may stand.",
                    ),
                    // The claim this exists to stop hubro making:
                    // the rollback was real, but it did not reach the schema
                    // changes, so "no changes were applied" would be false in
                    // the one direction that matters.
                    Rollback::OpenTransaction(open) => format!(
                        "Script stopped after {elapsed_ms} ms inside a transaction it had \
                         opened, and hubro rolled that transaction back. Statements before its \
                         BEGIN, and anything it committed, were not rolled back.{}",
                        schema_kept_note(open),
                    ),
                    Rollback::ExceptSchemaChanges => format!(
                        "Script rolled back after {elapsed_ms} ms, but this server does not undo \
                         schema changes on rollback — the schema changes it ran, and possibly \
                         data written before them, were kept. Check the schema before running \
                         it again.",
                    ),
                },
            }
        },
        RunStatus::Refused {
            reason,
            statement_index,
            preview,
        } => rsx! {
            // The same block as a failure, with the one thing a timing line
            // can't say: nothing reached the database, so there is no partial
            // state to reason about.
            StatementErrorBlock {
                statement_index,
                preview,
                message: reason,
                note: "Nothing was run — the script was refused before it reached the database."
                    .to_string(),
            }
        },
        RunStatus::Cancelled => rsx! {
            p { class: "border-t border-amber-300 dark:border-amber-900/50 px-4 py-3 text-sm text-amber-700 dark:text-amber-300",
                "Run cancelled — an in-flight statement may still complete on the server."
            }
        },
    }
}

/// What a run that ended inside its own transaction tells the user: the
/// transaction was rolled back, so its changes are gone.
fn left_open_note(open: OpenTransaction) -> String {
    format!(
        "The script ended inside a transaction it opened and never committed, so hubro \
         rolled it back — nothing it changed after its BEGIN was kept. A transaction can't \
         span runs: each run ends with none open, so run BEGIN … COMMIT together.{}",
        schema_kept_note(open),
    )
}

/// The caveat both open-transaction notes carry on a server whose rollback
/// does not reach schema changes — empty when it does, or nothing changed the
/// schema.
fn schema_kept_note(open: OpenTransaction) -> &'static str {
    if open.schema_changes_kept {
        " This server does not undo schema changes on rollback, so the schema changes it \
         ran were kept. Check the schema before running it again."
    } else {
        ""
    }
}

/// The count a statement section's header states after the preview.
fn outcome_summary(outcome: &StatementOutcome) -> String {
    match outcome {
        StatementOutcome::Affected(rows) => rows_affected_line(*rows),
        StatementOutcome::Rows(r) if r.rows.len() == 1 => "1 row".to_string(),
        StatementOutcome::Rows(r) => format!("{} rows", r.rows.len()),
    }
}

/// What a write reported, in words that claim no more than it did. A
/// [`RowsAffected::Summed`] count added up several, which on SQL Server can
/// be a trigger's as well as the statement's — and whether any were cannot
/// be told, so the line says "any" rather than naming triggers that may not
/// exist. [`RowsAffected::Unreported`] states no number: a 0 there would
/// claim nothing changed when a `SELECT … INTO #t` may have copied rows.
pub(super) fn rows_affected_line(rows: RowsAffected) -> String {
    let rows_word = |n: u64| if n == 1 { "row" } else { "rows" };
    match rows {
        RowsAffected::Exact(n) => format!("{n} {} affected", rows_word(n)),
        RowsAffected::Summed(n) => format!(
            "{n} {} affected in total, any triggers' rows included",
            rows_word(n)
        ),
        RowsAffected::Unreported => "no row count reported".to_string(),
    }
}

/// The error block both failing run outcomes render: which statement, its
/// preview, the message, and a footer saying what it means for the database.
///
/// Shares the [`Banner`] error palette but not its markup — a banner carries
/// one message, and the point here is the structure a single string can't
/// hold.
#[component]
fn StatementErrorBlock(
    statement_index: usize,
    preview: String,
    message: String,
    /// What the failure means for the database — rolled back, stopped
    /// part-way, or never sent.
    note: String,
) -> Element {
    rsx! {
        div { class: "m-3 flex items-start gap-2 rounded border px-3 py-2 {BannerKind::Error.container_classes()}",
            span { class: "shrink-0 select-none leading-5", {BannerKind::Error.icon()} }
            div { class: "min-w-0 flex-1",
                p { class: "mb-1 truncate font-mono text-xs opacity-80",
                    "{statement_index + 1} · {preview}"
                }
                p { class: "font-mono text-sm break-words", "{message}" }
                p { class: "mt-1 text-xs text-slate-500 dark:text-slate-400", "{note}" }
            }
        }
    }
}

/// One executed statement's section: a header naming the statement plus its
/// row count or affected count, and the result table for reads. Read results
/// carry an Export CSV/JSON control that serializes the full held
/// [`QueryResult`] (not just the rendered rows) through a native save dialog.
///
/// Takes the Arc-shared [`SharedStatement`], whose pointer-identity
/// `PartialEq` lets prop diffing skip this whole subtree while the statement
/// is unchanged — which is always, since results are write-once.
#[component]
fn StatementSection(
    id: ConnectionId,
    buffer: u64,
    index: usize,
    result: SharedStatement,
    /// How this statement's rest ended, when it was read after the run and
    /// has ended since.
    late: Option<LateRest>,
) -> Element {
    let state = use_context::<AppState>();
    let summary = outcome_summary(&result.outcome);
    // Whether there is a held result to export (reads only). The buttons
    // capture the shared statement, and the export borrows its rows through
    // the `Arc` — nothing is copied, per render or per click.
    let exportable =
        matches!(&result.outcome, StatementOutcome::Rows(rows) if !rows.rows.is_empty());
    let export_status: Option<ExportStatus> = sql_export_status(
        &state.export_status.read(),
        state.sql_export_source(id).as_ref(),
        id,
        &result,
    );
    rsx! {
        div { class: "border-b border-slate-200 dark:border-slate-800",
            p { class: "flex items-baseline gap-2 bg-slate-100 dark:bg-slate-900/60 px-4 py-1.5 text-xs",
                span { class: "font-mono text-slate-500", "{index}" }
                span { class: "min-w-0 truncate font-mono text-slate-900 dark:text-slate-300", "{result.preview}" }
                span { class: "shrink-0 text-cyan-700 dark:text-cyan-400", "— {summary}" }
                if exportable {
                    div { class: "flex-1" }
                    if let Some(status) = export_status.as_ref() {
                        {
                            let (text, tone) = status.line();
                            rsx! { span { class: "shrink-0 {tone.class()}", title: "{text}", "{text}" } }
                        }
                    }
                    for (format, label) in [
                        (ExportFormat::Csv, "CSV"),
                        (ExportFormat::Json, "JSON"),
                    ] {
                        button {
                            key: "{label}",
                            class: "shrink-0 {BTN_SECONDARY_SM}",
                            title: "Export this result to {label}",
                            onclick: {
                                let statement = result.clone();
                                move |_| {
                                    spawn_result_export(state, id, buffer, statement.clone(), format)
                                }
                            },
                            "Export {label}"
                        }
                    }
                }
            }
            // The result was capped at the fetch limit to bound memory
            // (distinct from the 500-row render cap inside ResultTable).
            if result.truncated {
                div { class: "px-4 pt-2",
                    Banner {
                        kind: BannerKind::Warning,
                        message: truncation_notice(result.rest, late.as_ref()),
                    }
                }
            }
            match &result.outcome {
                StatementOutcome::Affected(_) => rsx! {},
                StatementOutcome::Rows(rows) if rows.rows.is_empty() => rsx! {
                    p { class: "px-4 py-2 text-sm text-slate-500", "The statement returned no rows." }
                },
                StatementOutcome::Rows(_) => rsx! {
                    // Handed the whole shared statement, not the QueryResult
                    // inside it: the Arc bump keeps the prop pointer-diffed
                    // where a `rows.clone()` would deep-copy every row.
                    ResultTable { statement: result.clone() }
                },
            }
        }
    }
}

/// Opens a native save dialog and, on a chosen path, writes the held
/// result to it in `format` via [`AppState::export_result`] (a background
/// task; the UI never blocks).
///
/// `spawn_forever`, not the component-scoped `spawn` the grid's dialogs use:
/// the dialog does not block the window, and a rerun meanwhile empties the
/// results while the new run is in flight, unmounting the section that asked.
/// With a scoped task the dialog stayed open, accepted a path, and nothing
/// was written or reported — measured. The task holds the result it was asked
/// to export, so finishing it costs nothing, and its line lands under the run
/// ([`orphaned_sql_export_status`]).
fn spawn_result_export(
    state: AppState,
    id: ConnectionId,
    buffer: u64,
    statement: SharedStatement,
    format: ExportFormat,
) {
    dioxus::core::spawn_forever(async move {
        let picked = pick_export_path("Export query result", "query-result", format).await;
        if let Some(path) = picked {
            state.export_result(id, buffer, statement, format, path);
        }
    });
}

/// A result grid, capped at [`MAX_RENDERED_ROWS`] rendered rows.
///
/// Takes the shared statement rather than the [`QueryResult`] inside it: an
/// `Arc` cannot lend out a piece of itself, so passing the rows alone would
/// mean cloning them, and the pointer-eq `PartialEq` gating re-renders would
/// be lost with them.
#[component]
fn ResultTable(statement: SharedStatement) -> Element {
    let StatementOutcome::Rows(result) = &statement.outcome else {
        // Unreachable: StatementSection only mounts this for row outcomes.
        return rsx! {};
    };
    rsx! {
        if result.rows.len() > MAX_RENDERED_ROWS {
            p { class: "border-b border-amber-300 dark:border-amber-900/50 bg-amber-100 dark:bg-amber-950/30 px-4 py-1.5 text-xs text-amber-700 dark:text-amber-300",
                "Showing the first {MAX_RENDERED_ROWS} of {result.rows.len()} rows."
            }
        }
        table { class: "w-full border-collapse text-left",
            thead { class: "sticky top-0 bg-slate-100 dark:bg-slate-900",
                tr {
                    for column in result.columns.iter() {
                        th { class: "border-b border-slate-300 dark:border-slate-700 px-3 py-1.5 font-mono text-xs font-semibold text-slate-900 dark:text-slate-300",
                            "{column.name}"
                        }
                    }
                }
            }
            tbody {
                for row in result.rows.iter().take(MAX_RENDERED_ROWS) {
                    tr { class: "border-t border-slate-200 dark:border-slate-800/60 hover:bg-slate-100 dark:hover:bg-slate-800/30",
                        // Cells are a plain function, not a `#[component]`:
                        // a component instance per cell — each cloning its
                        // Value into an owned prop — was most of the render
                        // cost of a wide result.
                        for value in row.iter() {
                            {result_cell(value)}
                        }
                    }
                }
            }
        }
    }
}

/// How much of one cell's text a result grid puts into the DOM. The cell
/// shows one truncated line and its tooltip the start of the value; the text
/// goes in twice (content and `title`), and a typed `EXPLAIN` that is run
/// rather than explained can return a plan cell of up to
/// [`PLAN_CELL_CAP`] — sending 16 MiB to the webview to draw one line. The
/// held result keeps the whole value, which is what an export writes.
const RENDERED_CELL_BYTES: usize = 4 * 1024;

/// `text` cut to at most `max` bytes at a character boundary, marked with an
/// ellipsis when anything was cut.
fn rendered_text(mut text: String, max: usize) -> String {
    if text.len() > max {
        let mut end = max;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
        text.push('…');
    }
    text
}

/// One result cell. NULL renders as an italic marker — distinct from an empty
/// string — and blobs as their size tag, both via [`Value::display`].
fn result_cell(value: &Value) -> Element {
    let display = rendered_text(value.display(), RENDERED_CELL_BYTES);
    let class = match value {
        Value::Null => "px-3 py-1 font-mono text-xs italic text-slate-400 dark:text-slate-600",
        Value::Blob(_) => "px-3 py-1 font-mono text-xs text-violet-700 dark:text-violet-400",
        _ => "px-3 py-1 font-mono text-xs text-slate-900 dark:text-slate-200",
    };
    rsx! {
        td { class,
            div { class: "max-w-md truncate", title: "{display}", "{display}" }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_read_stopped_at_the_row_cap_says_what_became_of_its_rest() {
        let ran = truncation_notice(RestOfRead::Ran, None);
        assert!(!ran.contains("rest"));
        let stopped = truncation_notice(RestOfRead::Stopped, None);
        assert!(stopped.starts_with(&ran));
        assert!(stopped.ends_with("The rest was not run."));
        // The xid rule's line, and only there: a marked connection's read and
        // SQL Server's discarded one were not cancelled on its grounds.
        assert!(!stopped.contains("table rows"));
        let cancelled = truncation_notice(RestOfRead::CancelledUnwritten, None);
        assert!(cancelled.starts_with(&ran));
        assert!(cancelled.contains("The rest was cancelled at the row cap"));
        // What a cancel costs, as the user decided it should be said: the
        // whole statement undone, and a check that looks at table writes only.
        assert!(cancelled.contains("undid everything the statement did"));
        assert!(cancelled.contains("Only a statement that had written no table rows"));
        assert!(cancelled.contains("settings or notifications it made are undone too"));
        // A rest read after the run says that its outcome is not known, and
        // what a failure would cost.
        let unconfirmed = truncation_notice(RestOfRead::Unconfirmed, None);
        assert!(unconfirmed.starts_with(&ran));
        // Shown on every capped last read on these engines, plain reads
        // included, until the background read ends and the note is replaced:
        // so it leads with the condition, says the outcome is still to come,
        // and claims only the writes a failure undoes (a call outside the
        // transaction is not).
        assert!(unconfirmed.ends_with(
            "The rest of this result is still being read after the run ended; this note will \
             say how it ends. If the statement wrote anything and the rest fails, its writes \
             are undone."
        ));
        assert!(!unconfirmed.contains("isn't reported"));
        assert!(!unconfirmed.contains("everything the statement did"));
        // Once the background read has ended, the notice says how: finished,
        // or failed with the server's error and the writes undone, or not
        // known when the connection was lost.
        let finished = truncation_notice(RestOfRead::Unconfirmed, Some(&LateRest::Finished));
        assert!(finished.starts_with(&ran));
        assert!(finished
            .ends_with("The rest of this result was read after the run ended and finished."));
        assert!(!finished.contains("undone"));
        let failed = truncation_notice(
            RestOfRead::Unconfirmed,
            Some(&LateRest::Failed("division by zero".into())),
        );
        assert!(failed.ends_with(
            "The rest of this result failed after the run ended: division by zero. If the \
             statement wrote anything, its writes were undone."
        ));
        let unknown = truncation_notice(RestOfRead::Unconfirmed, Some(&LateRest::Unknown));
        assert!(unknown.contains("its outcome is unknown"));
        assert!(unknown.contains("may have been undone"));
        assert!(!unknown.contains("finished") && !unknown.contains("failed"));
        // A late outcome speaks only for an unconfirmed rest.
        for rest in [
            RestOfRead::Ran,
            RestOfRead::Stopped,
            RestOfRead::CancelledUnwritten,
        ] {
            assert_eq!(
                truncation_notice(rest, Some(&LateRest::Failed("x".into()))),
                truncation_notice(rest, None)
            );
        }
    }

    /// Without a dialect the banner states no count at all, rather than one
    /// judged by a guessed engine; with one, the dialect's rule decides.
    #[test]
    fn a_pending_script_is_counted_only_by_a_known_dialect() {
        let script = ["select 1".to_string(), "delete from t".to_string()];
        assert_eq!(writes_to_confirm(&script, None), None);
        assert_eq!(writes_to_confirm(&script, Some(Dialect::Sqlite)), Some(1));
        assert_eq!(
            writes_to_confirm(&script[..1], Some(Dialect::Postgres)),
            Some(0)
        );
    }

    #[test]
    fn a_write_count_claims_no_more_than_the_driver_reported() {
        // The statement's own count: a plain number, no caveat.
        let exact = outcome_summary(&StatementOutcome::Affected(RowsAffected::Exact(1)));
        assert!(exact.starts_with("1 row affected"), "{exact}");
        assert!(!exact.contains("trigger"), "{exact}");
        assert!(!exact.contains("total"), "{exact}");

        // Several counts added up: says so, and that triggers may be in it.
        let summed = outcome_summary(&StatementOutcome::Affected(RowsAffected::Summed(4)));
        assert!(summed.starts_with("4 rows affected"), "{summed}");
        assert!(summed.contains("in total"), "{summed}");
        assert!(summed.contains("any triggers"), "{summed}");

        // No count: no number, so not "0 rows affected".
        let unreported = outcome_summary(&StatementOutcome::Affected(RowsAffected::Unreported));
        assert!(
            !unreported.chars().any(|c| c.is_ascii_digit()),
            "{unreported}"
        );
        assert!(unreported.contains("no row count"), "{unreported}");
    }

    #[test]
    fn a_rendered_cell_is_bounded_at_a_character_boundary() {
        assert_eq!(rendered_text("short".into(), 8), "short");
        assert_eq!(rendered_text("exactly8".into(), 8), "exactly8");
        assert_eq!(rendered_text("abcdefghij".into(), 8), "abcdefgh…");
        // A three-byte character straddling the bound is dropped whole.
        assert_eq!(rendered_text("abcdefg€".into(), 8), "abcdefg…");
        let plan = "x".repeat(PLAN_CELL_CAP);
        assert!(
            rendered_text(plan, RENDERED_CELL_BYTES).len() <= RENDERED_CELL_BYTES + '…'.len_utf8()
        );
    }

    /// The results a [`Sections`] tree shows, kept in the root scope so the
    /// test can swap them between renders.
    #[derive(Clone, Copy)]
    struct Shown(Signal<Vec<SharedStatement>>);

    /// One [`StatementSection`] per shown result, keyed by its preview, so a
    /// result that appears is mounted fresh and renders all of its text.
    #[component]
    fn Sections(id: ConnectionId) -> Element {
        let shown = use_context::<Shown>().0;
        rsx! {
            for (index, statement) in shown.read().iter().cloned().enumerate() {
                StatementSection { key: "{statement.preview}", id, buffer: 0, index, result: statement }
            }
        }
    }

    fn rows_statement(preview: &str) -> SharedStatement {
        SharedStatement::new(crate::db::StatementResult {
            preview: preview.into(),
            outcome: StatementOutcome::Rows(crate::db::QueryResult {
                columns: vec![crate::db::ColumnInfo { name: "a".into() }],
                rows: vec![vec![Value::Integer(1)]],
            }),
            truncated: false,
            rest: crate::db::RestOfRead::Ran,
        })
    }

    /// The whole wire of a SQL-result export's status line, through the real
    /// `export_result` and the real section: the line is drawn beside the
    /// result that was exported and nowhere else — not beside the other
    /// result of the same run, and not beside a later run's identical one.
    /// Every section used to read the connection's one status, so both
    /// results of a two-`SELECT` run claimed the export.
    #[tokio::test]
    async fn an_export_status_shows_beside_the_exported_result_alone() {
        use crate::db::{ConnectionRegistry, DbPool, WriteProtection};
        use crate::ui::state::ExportPane;
        use crate::ui::test_dom::{provide_state, texts};
        use dioxus::core::{NoOpMutations, VirtualDom};

        let pool =
            DbPool::Sqlite(sqlx::sqlite::SqlitePool::connect_lazy("sqlite::memory:").unwrap());
        let id = ConnectionRegistry::default().insert("t.db", pool.clone(), WriteProtection::Open);
        let mut dom = VirtualDom::new_with_props(Sections, SectionsProps { id });
        let state = provide_state(&dom);
        // Open in the state's own registry too, under the same id (a fresh
        // registry's first): what an export records about a tab is kept only
        // while the tab is open.
        let open = dom.in_runtime(|| {
            let mut registry = state.registry;
            let open = registry.write().insert("t.db", pool, WriteProtection::Open);
            open
        });
        assert_eq!(open, id);
        let shown = dom.in_runtime(|| Shown(Signal::new_in_scope(Vec::new(), ScopeId::ROOT)));
        dom.provide_root_context(shown);
        dom.rebuild(&mut NoOpMutations);

        let (first, second) = (rows_statement("first"), rows_statement("second"));
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("out.csv");
        dom.in_runtime(|| {
            state.export_result(id, 0, first.clone(), ExportFormat::Csv, path.clone())
        });
        let done = Some(ExportStatus::Done { rows: 1 });
        let finished = tokio::time::timeout(std::time::Duration::from_secs(10), async {
            while dom.in_runtime(|| {
                state
                    .export_status
                    .peek()
                    .get(&(id, ExportPane::Sql))
                    .cloned()
            }) != done
            {
                // Bounded: once the task has landed its status there is no
                // work left to wait for, and the check above must run again.
                let _ =
                    tokio::time::timeout(std::time::Duration::from_millis(50), dom.wait_for_work())
                        .await;
            }
        })
        .await;
        assert!(
            finished.is_ok(),
            "the export never finished: {:?}",
            dom.in_runtime(|| state
                .export_status
                .peek()
                .get(&(id, ExportPane::Sql))
                .cloned())
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap().lines().count(), 2);

        let mut show = |statements: Vec<SharedStatement>| {
            let mut shown = shown.0;
            dom.in_runtime(|| shown.set(Vec::new()));
            dom.render_immediate(&mut NoOpMutations);
            dom.in_runtime(|| shown.set(statements));
            let edits = dom.render_immediate_to_vec().edits;
            texts(&edits)
                .iter()
                .filter(|text| text.starts_with("Exported"))
                .count()
        };
        assert_eq!(show(vec![first.clone(), second.clone()]), 1);
        assert_eq!(show(vec![second.clone()]), 0);
        assert_eq!(show(vec![first.clone()]), 1);
        // A rerun's result with the same preview and rows is not the one
        // the file was written from.
        assert_eq!(show(vec![rows_statement("first")]), 0);
    }
}
