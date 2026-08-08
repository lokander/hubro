use chrono::{DateTime, Local};
use dioxus::prelude::*;
use dioxus_icons::lucide::Play;
use serde::Deserialize;

use crate::db::{
    needs_confirmation, ConnectionId, Dialect, ExportFormat, QueryResult, StatementOutcome,
    StatementResult, TableMeta, Value, MARKED_READ_ONLY, MAX_QUERY_ROWS, NO_DDL, NO_MUTATE,
    NO_QUERY,
};
use crate::history::{HistoryEntry, HISTORY_CAP};

use super::notice::{Banner, BannerKind, EmptyState, LoadingLine};
use super::state::{AppState, ExportPane, ExportStatus, RunStatus, SchemaLoad};

/// Cap on rendered result rows (per statement). The full result still sits
/// in memory until FRE-33 introduces streaming/limits at the query layer,
/// but the DOM must not receive a million rows.
const MAX_RENDERED_ROWS: usize = 500;

/// Messages the CodeMirror bundle sends over the eval channel.
#[derive(Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
enum EditorMessage {
    Run { sql: String },
    Doc { doc: String },
}

/// Free-form SQL pane: CodeMirror 6 editor (bundled locally in
/// assets/codemirror.js — the app works offline) above, results below.
/// Ctrl+Enter runs the buffer, or just the selection when one exists.
#[component]
pub fn SqlEditor(id: ConnectionId) -> Element {
    let state = use_context::<AppState>();
    let dialect = state
        .registry
        .read()
        .get(id)
        .map(|c| c.pool.dialect())
        .unwrap_or(Dialect::Sqlite);
    let editor_element = format!("sql-editor-{id:?}").replace(['(', ')'], "-");
    let dialect_name = match dialect {
        Dialect::Postgres => "postgres",
        Dialect::Sqlite => "sqlite",
        Dialect::SqlServer => "mssql",
    };

    // Mount CodeMirror once and pump its messages. The eval channel stays
    // open for the component's lifetime; unmounting destroys the JS view.
    let element_for_effect = editor_element.clone();
    use_effect(move || {
        let element = element_for_effect.clone();
        let initial = state
            .tab_ui
            .peek()
            .get(&id)
            .map(|ui| ui.sql_text.clone())
            .unwrap_or_default();
        // Whatever the introspection has produced so far; the refresh
        // effect below pushes updates once (re)loads finish.
        let schema_json = match state.schemas.peek().get(&id) {
            Some(SchemaLoad::Ready(tables)) => completion_schema(tables, dialect).to_string(),
            _ => "{}".to_string(),
        };
        let initial_json = serde_json::to_string(&initial).unwrap_or_else(|_| "\"\"".into());
        spawn(async move {
            let js = format!(
                r#"
                window.__dvRun = (p) => dioxus.send(JSON.stringify({{ kind: "run", sql: p.sql }}));
                window.__dvDoc = (p) => dioxus.send(JSON.stringify({{ kind: "doc", doc: p.doc }}));
                DVEditor.create("{element}", "{element}", "{dialect_name}", {initial_json}, {schema_json});
                "#
            );
            let mut channel = document::eval(&js);
            // The channel closes (Err) when the component unmounts.
            while let Ok(raw) = channel.recv::<String>().await {
                match serde_json::from_str::<EditorMessage>(&raw) {
                    Ok(EditorMessage::Run { sql }) => {
                        let trimmed = sql.trim();
                        if !trimmed.is_empty() {
                            state.run_sql(id, trimmed.to_string());
                        }
                    }
                    Ok(EditorMessage::Doc { doc }) => state.set_sql_text(id, doc),
                    Err(_) => {}
                }
            }
        });
    });

    // Keep completion data in sync with schema reloads: reading the signal
    // (not peeking) subscribes this effect, so it re-runs whenever
    // `load_schema` rewrites the entry. While a reload is in flight
    // (Loading) or failed, the editor keeps its previous completions.
    let element_for_schema = editor_element.clone();
    use_effect(move || {
        let schema_json = match state.schemas.read().get(&id) {
            Some(SchemaLoad::Ready(tables)) => completion_schema(tables, dialect).to_string(),
            _ => return,
        };
        document::eval(&format!(
            r#"DVEditor.updateSchema("{element_for_schema}", "{dialect_name}", {schema_json});"#
        ));
    });

    let element_for_drop = editor_element.clone();
    use_drop(move || {
        document::eval(&format!(r#"DVEditor.destroy("{element_for_drop}");"#));
    });

    let run = state.sql_runs.read().get(&id).cloned();
    let running = matches!(run.as_ref().map(|r| &r.status), Some(RunStatus::Running));
    // What this connection won't run, stated before the user writes it
    // (FRE-87). `run_sql` refuses the same cases, so the note explains a
    // refusal that would otherwise arrive only after pressing Ctrl+Enter.
    //
    // Effective capabilities, so a connection the user marked read-only
    // (FRE-111) says so here rather than only when the run is refused.
    let capability_note: Option<&'static str> = match state.connection_caps(id) {
        Some(caps) if !caps.read_query => Some(NO_QUERY),
        Some(caps) if !caps.mutate => Some(if state.marked_read_only(id) {
            MARKED_READ_ONLY
        } else {
            NO_MUTATE
        }),
        Some(caps) if !caps.ddl => Some(NO_DDL),
        _ => None,
    };
    let pending_writes = state.pending_sql.read().get(&id).map(|pending| {
        pending
            .statements
            .iter()
            .filter(|s| needs_confirmation(s, dialect))
            .count()
    });
    // Under Confirm (FRE-111) the banner names the connection: the whole
    // point of the state is to make you read *which* database you are about
    // to change, which "Run anyway?" alone never made you do.
    let confirm_target: Option<String> = state
        .confirms_writes(id)
        .then(|| state.registry.read().get(id).map(|c| c.name.clone()))
        .flatten();
    let mut show_history = use_signal(|| false);

    rsx! {
        div { class: "flex h-full min-h-0",
        div { class: "flex min-w-0 flex-1 flex-col",
            div { class: "flex items-center justify-between border-b border-slate-200 dark:border-slate-800 px-3 py-1.5",
                div { class: "flex min-w-0 items-center gap-2",
                    span { class: "text-xs text-slate-500",
                        "Ctrl+Enter runs the buffer — or just the selection."
                    }
                    if let Some(note) = capability_note {
                        span {
                            class: "truncate rounded border border-amber-300 dark:border-amber-900/50 px-1.5 py-0.5 text-xs text-amber-700 dark:text-amber-300",
                            title: "{note}",
                            "{note}"
                        }
                    }
                }
                div { class: "flex items-center gap-2",
                    if running {
                        button {
                            class: "rounded border border-rose-300 dark:border-rose-800 px-2 py-0.5 text-xs text-rose-700 dark:text-rose-300 hover:bg-rose-100 dark:hover:bg-rose-950/50",
                            onclick: move |_| state.cancel_sql(id),
                            "Cancel"
                        }
                    }
                    button {
                        class: if show_history() {
                            "rounded bg-slate-300 dark:bg-slate-700 px-2 py-0.5 text-xs text-slate-900 dark:text-slate-100"
                        } else {
                            "rounded border border-slate-300 dark:border-slate-700 px-2 py-0.5 text-xs text-slate-500 dark:text-slate-400 hover:bg-slate-200 dark:hover:bg-slate-800 hover:text-slate-900 dark:hover:text-slate-100"
                        },
                        onclick: move |_| {
                            let showing = *show_history.read();
                            show_history.set(!showing);
                        },
                        "History"
                    }
                }
            }
            div {
                id: "{editor_element}",
                class: "h-1/2 min-h-0 shrink-0 overflow-hidden border-b border-slate-300 dark:border-slate-700 text-sm",
            }
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
                            onclick: move |_| state.confirm_pending_sql(id),
                            "Run"
                        }
                        button {
                            class: "rounded border border-slate-400 dark:border-slate-600 px-2.5 py-0.5 text-xs text-slate-900 dark:text-slate-300 hover:bg-slate-200 dark:hover:bg-slate-800",
                            onclick: move |_| state.dismiss_pending_sql(id),
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
                            StatementSection {
                                key: "{index}",
                                id,
                                index: index + 1,
                                result: statement.clone(),
                            }
                        }
                        RunStatusLine { status: run.status.clone(), statement_count: run.statements.len() }
                    },
                }
            }
        }
        if show_history() {
            HistoryPanel { id, editor_element: editor_element.clone() }
        }
        }
    }
}

/// Cap on entries the history panel fetches per query. Matches the store's
/// retention cap so every persisted entry is reachable by scrolling, not only
/// via search (FRE-42).
const HISTORY_PANEL_LIMIT: i64 = HISTORY_CAP;

/// Right-side panel listing the current connection's persisted query
/// history: search, per-entry Load / Run / Copy, clear-history, and the
/// recording opt-out.
#[component]
fn HistoryPanel(id: ConnectionId, editor_element: String) -> Element {
    let state = use_context::<AppState>();
    // The controlled input; `search` only follows it on Enter (or when the
    // input is cleared) so each keystroke doesn't hit the database.
    let mut search_input = use_signal(String::new);
    let mut search = use_signal(String::new);
    let mut confirm_clear = use_signal(|| false);
    let store_ready = state.history.read().is_some();
    let history_error = state.history_error.read().clone();
    let mut record_error_signal = state.history_record_error;
    let record_error = record_error_signal.read().clone();
    let recording = *state.history_recording.read();

    let entries = use_resource(move || {
        // Reactive dependencies, all cloned out before the await.
        let store = state.history.read().clone();
        let _refresh = *state.history_nonce.read();
        let needle = search.read().clone();
        let locator = state
            .open_locators
            .read()
            .iter()
            .find(|(open_id, _)| *open_id == id)
            .map(|(_, locator)| locator.clone());
        async move {
            let (Some(store), Some(locator)) = (store, locator) else {
                return Ok(Vec::new());
            };
            store
                .list(&locator, Some(&needle), HISTORY_PANEL_LIMIT)
                .await
        }
    });
    let entries = entries.read().clone();

    rsx! {
        aside { class: "flex w-80 shrink-0 flex-col border-l border-slate-300 dark:border-slate-700 bg-slate-50 dark:bg-slate-950/50",
            div { class: "border-b border-slate-200 dark:border-slate-800 p-2",
                input {
                    class: "w-full rounded border border-slate-300 dark:border-slate-700 bg-slate-100 dark:bg-slate-950 px-2 py-1 text-xs text-slate-900 dark:text-slate-200 placeholder:text-slate-400 dark:placeholder:text-slate-600",
                    placeholder: "Search history (Enter)",
                    value: "{search_input}",
                    oninput: move |evt| {
                        let value = evt.value();
                        // Clearing the box resets the filter immediately.
                        if value.trim().is_empty() {
                            search.set(String::new());
                        }
                        search_input.set(value);
                    },
                    onkeydown: move |evt: KeyboardEvent| {
                        if evt.key() == Key::Enter {
                            search.set(search_input.peek().clone());
                        }
                    },
                }
            }
            div { class: "min-h-0 flex-1 overflow-y-auto",
                // A failed write for a completed run: warn without hiding
                // the still-readable list (FRE-72).
                if let Some(err) = record_error {
                    div { class: "p-2",
                        Banner {
                            kind: BannerKind::Warning,
                            message: err,
                            on_dismiss: move |_| record_error_signal.set(None),
                        }
                    }
                }
                if let Some(err) = history_error {
                    div { class: "p-2",
                        Banner { kind: BannerKind::Warning, message: format!("History is unavailable: {err}") }
                    }
                } else if !store_ready {
                    LoadingLine { label: "Opening history…" }
                } else {
                    match entries {
                        None => rsx! {
                            LoadingLine { label: "Loading…" }
                        },
                        Some(Err(err)) => rsx! {
                            div { class: "p-2",
                                Banner { kind: BannerKind::Warning, message: format!("History query failed: {err}") }
                            }
                        },
                        Some(Ok(entries)) if entries.is_empty() => rsx! {
                            p { class: "px-3 py-2 text-xs text-slate-500",
                                if search.read().trim().is_empty() {
                                    "No queries recorded yet."
                                } else {
                                    "No matches."
                                }
                            }
                        },
                        Some(Ok(entries)) => rsx! {
                            ul { class: "divide-y divide-slate-200 dark:divide-slate-800/60",
                                for entry in entries {
                                    HistoryRow {
                                        key: "{entry.id}",
                                        id,
                                        editor_element: editor_element.clone(),
                                        entry,
                                    }
                                }
                            }
                        },
                    }
                }
            }
            div { class: "flex flex-col gap-2 border-t border-slate-200 dark:border-slate-800 p-2",
                label { class: "flex items-center gap-2 text-xs text-slate-500 dark:text-slate-400",
                    input {
                        r#type: "checkbox",
                        checked: recording,
                        disabled: !store_ready,
                        onchange: move |evt| state.set_history_recording(evt.checked()),
                    }
                    "Record executed queries"
                }
                if confirm_clear() {
                    div { class: "flex items-center gap-2",
                        span { class: "text-xs text-amber-700 dark:text-amber-300", "Clear this connection's history?" }
                        button {
                            class: "rounded bg-amber-600 px-2 py-0.5 text-xs font-semibold text-slate-950 hover:bg-amber-500",
                            onclick: move |_| {
                                state.clear_history(id);
                                confirm_clear.set(false);
                            },
                            "Clear"
                        }
                        button {
                            class: "rounded border border-slate-400 dark:border-slate-600 px-2 py-0.5 text-xs text-slate-900 dark:text-slate-300 hover:bg-slate-200 dark:hover:bg-slate-800",
                            onclick: move |_| confirm_clear.set(false),
                            "Keep"
                        }
                    }
                } else {
                    button {
                        class: "self-start rounded border border-slate-300 dark:border-slate-700 px-2 py-0.5 text-xs text-slate-500 dark:text-slate-400 hover:bg-slate-200 dark:hover:bg-slate-800 hover:text-slate-900 dark:hover:text-slate-100",
                        disabled: !store_ready,
                        onclick: move |_| confirm_clear.set(true),
                        "Clear history"
                    }
                }
            }
        }
    }
}

/// One history entry: status dot, local-time stamp, one-line SQL preview
/// (full text in the tooltip), and Load / Run / Copy actions.
#[component]
fn HistoryRow(id: ConnectionId, editor_element: String, entry: HistoryEntry) -> Element {
    let state = use_context::<AppState>();
    let time = format_history_time(entry.executed_at);
    let preview: String = entry.sql.split_whitespace().collect::<Vec<_>>().join(" ");
    let sql_json = serde_json::to_string(&entry.sql).unwrap_or_else(|_| "\"\"".into());
    let sql_for_load = entry.sql.clone();
    let json_for_load = sql_json.clone();
    let sql_for_run = entry.sql.clone();
    let json_for_run = sql_json.clone();
    let element_for_run = editor_element.clone();
    let title = match &entry.error {
        Some(error) => format!("{}\n\n{error}", entry.sql),
        None => entry.sql.clone(),
    };
    rsx! {
        li { class: "px-3 py-2", title: "{title}",
            div { class: "flex items-center gap-2",
                span {
                    class: if entry.success {
                        "h-2 w-2 shrink-0 rounded-full bg-emerald-400"
                    } else {
                        "h-2 w-2 shrink-0 rounded-full bg-red-400"
                    },
                }
                span { class: "shrink-0 text-xs text-slate-500", "{time}" }
                span { class: "min-w-0 flex-1 truncate font-mono text-xs text-slate-900 dark:text-slate-300",
                    "{preview}"
                }
            }
            div { class: "mt-1 flex gap-1 pl-4",
                button {
                    class: "rounded border border-slate-300 dark:border-slate-700 px-1.5 py-0.5 text-xs text-slate-500 dark:text-slate-400 hover:bg-slate-200 dark:hover:bg-slate-800 hover:text-slate-900 dark:hover:text-slate-100",
                    onclick: move |_| {
                        document::eval(&format!(
                            r#"DVEditor.setDoc("{editor_element}", {json_for_load});"#
                        ));
                        state.set_sql_text(id, sql_for_load.clone());
                    },
                    "Load"
                }
                button {
                    class: "rounded border border-slate-300 dark:border-slate-700 px-1.5 py-0.5 text-xs text-cyan-700 dark:text-cyan-300 hover:bg-slate-200 dark:hover:bg-slate-800",
                    // Load into the editor buffer first so a write-confirm
                    // banner is confirmed against the text on screen (FRE-72).
                    onclick: move |_| {
                        document::eval(&format!(
                            r#"DVEditor.setDoc("{element_for_run}", {json_for_run});"#
                        ));
                        state.set_sql_text(id, sql_for_run.clone());
                        state.run_sql(id, sql_for_run.clone());
                    },
                    "Run"
                }
                button {
                    class: "rounded border border-slate-300 dark:border-slate-700 px-1.5 py-0.5 text-xs text-slate-500 dark:text-slate-400 hover:bg-slate-200 dark:hover:bg-slate-800 hover:text-slate-900 dark:hover:text-slate-100",
                    onclick: move |_| {
                        document::eval(&format!(
                            "navigator.clipboard.writeText({sql_json});"
                        ));
                    },
                    "Copy"
                }
            }
        }
    }
}

/// Builds the completion namespace handed to lang-sql (its `SQLNamespace`
/// shape): an object mapping table names — `"table"` on SQLite,
/// `"schema.table"` on Postgres and SQL Server — to arrays of column names.
/// lang-sql splits keys on unescaped dots, so literal dots inside
/// identifiers are escaped as `\.`; it also quote-applies any completion
/// whose label needs quoting, so weird identifiers can be passed through as
/// plain strings.
fn completion_schema(tables: &[TableMeta], dialect: Dialect) -> serde_json::Value {
    let mut namespace = serde_json::Map::new();
    for table in tables {
        let key = match (&dialect, &table.schema) {
            (Dialect::Postgres | Dialect::SqlServer, Some(schema)) => {
                format!("{}.{}", escape_dots(schema), escape_dots(&table.name))
            }
            _ => escape_dots(&table.name),
        };
        let columns: Vec<serde_json::Value> = table
            .columns
            .iter()
            .map(|c| serde_json::Value::String(c.name.clone()))
            .collect();
        namespace.insert(key, serde_json::Value::Array(columns));
    }
    serde_json::Value::Object(namespace)
}

/// Escapes literal dots in an identifier for use in an `SQLNamespace` key.
fn escape_dots(name: &str) -> String {
    name.replace('.', "\\.")
}

/// Formats a unix timestamp in local time: bare `HH:MM:SS` for today,
/// date-prefixed for older entries.
fn format_history_time(unix_secs: i64) -> String {
    let Some(utc) = DateTime::from_timestamp(unix_secs, 0) else {
        return String::new();
    };
    let local = utc.with_timezone(&Local);
    if local.date_naive() == Local::now().date_naive() {
        local.format("%H:%M:%S").to_string()
    } else {
        local.format("%Y-%m-%d %H:%M").to_string()
    }
}

/// The script-level footer: running indicator, total elapsed time (shown
/// once per run), or the failure block.
#[component]
fn RunStatusLine(status: RunStatus, statement_count: usize) -> Element {
    match status {
        RunStatus::Running => rsx! {
            LoadingLine { label: "Running…" }
        },
        RunStatus::Done { elapsed_ms } => rsx! {
            p { class: "px-4 py-2 text-xs text-slate-500",
                if statement_count == 1 {
                    "1 statement in {elapsed_ms} ms"
                } else {
                    "{statement_count} statements in {elapsed_ms} ms"
                }
            }
        },
        RunStatus::Failed {
            error,
            statement_index,
            preview,
            elapsed_ms,
            rolled_back,
        } => rsx! {
            // Shares the Banner error palette; keeps the structured
            // statement/error/note layout a single message can't hold.
            div { class: "m-3 flex items-start gap-2 rounded border px-3 py-2 {BannerKind::Error.container_classes()}",
                span { class: "shrink-0 select-none leading-5", {BannerKind::Error.icon()} }
                div { class: "min-w-0 flex-1",
                    p { class: "mb-1 truncate font-mono text-xs opacity-80",
                        "{statement_index + 1} · {preview}"
                    }
                    p { class: "font-mono text-sm break-words", "{error}" }
                    p { class: "mt-1 text-xs text-slate-500 dark:text-slate-400",
                        if rolled_back {
                            "Script rolled back after {elapsed_ms} ms — no changes were applied."
                        } else {
                            "Script stopped after {elapsed_ms} ms; earlier statements were not rolled back."
                        }
                    }
                }
            }
        },
        RunStatus::Refused {
            reason,
            statement_index,
            preview,
        } => rsx! {
            // Same layout as a failure, but the footer states the one thing
            // that matters and a timing line can't: nothing reached the
            // database, so there is no partial state to reason about.
            div { class: "m-3 flex items-start gap-2 rounded border px-3 py-2 {BannerKind::Error.container_classes()}",
                span { class: "shrink-0 select-none leading-5", {BannerKind::Error.icon()} }
                div { class: "min-w-0 flex-1",
                    p { class: "mb-1 truncate font-mono text-xs opacity-80",
                        "{statement_index + 1} · {preview}"
                    }
                    p { class: "font-mono text-sm break-words", "{reason}" }
                    p { class: "mt-1 text-xs text-slate-500 dark:text-slate-400",
                        "Nothing was run — the script was refused before it reached the database."
                    }
                }
            }
        },
        RunStatus::Cancelled => rsx! {
            p { class: "border-t border-amber-300 dark:border-amber-900/50 px-4 py-3 text-sm text-amber-700 dark:text-amber-300",
                "Run cancelled — an in-flight statement may still complete on the server."
            }
        },
    }
}

/// One executed statement's section: a header naming the statement plus its
/// row count or affected count, and the result table for reads. Read results
/// carry an Export CSV/JSON control that serializes the full held
/// [`QueryResult`] (not just the rendered rows) through a native save dialog.
#[component]
fn StatementSection(id: ConnectionId, index: usize, result: StatementResult) -> Element {
    let state = use_context::<AppState>();
    let summary = match &result.outcome {
        StatementOutcome::Affected(1) => "1 row affected".to_string(),
        StatementOutcome::Affected(n) => format!("{n} rows affected"),
        StatementOutcome::Rows(r) if r.rows.len() == 1 => "1 row".to_string(),
        StatementOutcome::Rows(r) => format!("{} rows", r.rows.len()),
    };
    // The held result to export (reads only). Cloned once here so the export
    // buttons can hand a snapshot to the background task.
    let exportable: Option<QueryResult> = match &result.outcome {
        StatementOutcome::Rows(rows) if !rows.rows.is_empty() => Some(rows.clone()),
        _ => None,
    };
    let export_status: Option<ExportStatus> = state
        .export_status
        .read()
        .get(&(id, ExportPane::Sql))
        .cloned();
    rsx! {
        div { class: "border-b border-slate-200 dark:border-slate-800",
            p { class: "flex items-baseline gap-2 bg-slate-100 dark:bg-slate-900/60 px-4 py-1.5 text-xs",
                span { class: "font-mono text-slate-500", "{index}" }
                span { class: "min-w-0 truncate font-mono text-slate-900 dark:text-slate-300", "{result.preview}" }
                span { class: "shrink-0 text-cyan-700 dark:text-cyan-400", "— {summary}" }
                if let Some(result) = exportable {
                    div { class: "flex-1" }
                    if let Some(status) = export_status.as_ref() {
                        {
                            let (text, class) = status.line();
                            rsx! { span { class: "shrink-0 {class}", title: "{text}", "{text}" } }
                        }
                    }
                    button {
                        class: "shrink-0 rounded border border-slate-300 dark:border-slate-700 px-1.5 py-0.5 text-slate-500 dark:text-slate-400 hover:bg-slate-200 dark:hover:bg-slate-800 hover:text-slate-900 dark:hover:text-slate-100",
                        title: "Export this result to CSV",
                        onclick: {
                            let result = result.clone();
                            move |_| spawn_result_export(state, id, result.clone(), ExportFormat::Csv)
                        },
                        "Export CSV"
                    }
                    button {
                        class: "shrink-0 rounded border border-slate-300 dark:border-slate-700 px-1.5 py-0.5 text-slate-500 dark:text-slate-400 hover:bg-slate-200 dark:hover:bg-slate-800 hover:text-slate-900 dark:hover:text-slate-100",
                        title: "Export this result to JSON",
                        onclick: {
                            let result = result.clone();
                            move |_| spawn_result_export(state, id, result.clone(), ExportFormat::Json)
                        },
                        "Export JSON"
                    }
                }
            }
            // The result was capped at the fetch limit to bound memory
            // (distinct from the 500-row render cap inside ResultTable).
            if result.truncated {
                div { class: "px-4 pt-2",
                    Banner {
                        kind: BannerKind::Warning,
                        message: format!(
                            "Result truncated — showing the first {MAX_QUERY_ROWS} rows to keep memory bounded. \
                             Add a LIMIT or a WHERE clause to narrow it.",
                        ),
                    }
                }
            }
            match &result.outcome {
                StatementOutcome::Affected(_) => rsx! {},
                StatementOutcome::Rows(rows) if rows.rows.is_empty() => rsx! {
                    p { class: "px-4 py-2 text-sm text-slate-500", "The statement returned no rows." }
                },
                StatementOutcome::Rows(rows) => rsx! {
                    ResultTable { result: rows.clone() }
                },
            }
        }
    }
}

/// Opens a native save dialog and, on a chosen path, writes the held
/// [`QueryResult`] to it in `format` via [`AppState::export_result`] (a
/// background task; the UI never blocks).
fn spawn_result_export(
    state: AppState,
    id: ConnectionId,
    result: QueryResult,
    format: ExportFormat,
) {
    let (filter_name, ext) = match format {
        ExportFormat::Csv => ("CSV", "csv"),
        ExportFormat::Json => ("JSON", "json"),
    };
    let suggested = format!("query-result.{ext}");
    spawn(async move {
        let picked = rfd::AsyncFileDialog::new()
            .set_title("Export query result")
            .set_file_name(suggested)
            .add_filter(filter_name, &[ext])
            .save_file()
            .await;
        if let Some(file) = picked {
            state.export_result(id, result, format, file.path().to_path_buf());
        }
    });
}

/// A result grid, capped at [`MAX_RENDERED_ROWS`] rendered rows.
#[component]
fn ResultTable(result: QueryResult) -> Element {
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
                        for value in row.iter() {
                            ResultCell { value: value.clone() }
                        }
                    }
                }
            }
        }
    }
}

#[component]
fn ResultCell(value: Value) -> Element {
    let display = value.display();
    let class = match &value {
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
    use crate::db::{ColumnMeta, TableKind};
    use serde_json::json;

    fn table(schema: Option<&str>, name: &str, columns: &[&str]) -> TableMeta {
        TableMeta {
            schema: schema.map(Into::into),
            name: name.into(),
            kind: TableKind::Table,
            columns: columns
                .iter()
                .map(|c| ColumnMeta {
                    name: (*c).into(),
                    type_name: "TEXT".into(),
                    nullable: true,
                    primary_key_position: None,
                    default: None,
                    generated: crate::db::Generated::Never,
                    type_detail: crate::db::TypeDetail::Plain,
                })
                .collect(),
            indexes: vec![],
            foreign_keys: vec![],
            restriction: None,
        }
    }

    #[test]
    fn sqlite_schema_uses_flat_table_names() {
        let tables = [
            table(None, "artists", &["id", "name"]),
            table(None, "albums", &["id", "artist_id", "title"]),
        ];
        assert_eq!(
            completion_schema(&tables, Dialect::Sqlite),
            json!({
                "artists": ["id", "name"],
                "albums": ["id", "artist_id", "title"],
            })
        );
    }

    #[test]
    fn postgres_schema_qualifies_table_names() {
        let tables = [
            table(Some("public"), "users", &["id", "email"]),
            table(Some("audit"), "events", &["id", "at"]),
        ];
        assert_eq!(
            completion_schema(&tables, Dialect::Postgres),
            json!({
                "public.users": ["id", "email"],
                "audit.events": ["id", "at"],
            })
        );
    }

    #[test]
    fn sqlserver_schema_qualifies_table_names() {
        let tables = [
            table(Some("dbo"), "users", &["id", "email"]),
            table(None, "loners", &["id"]),
        ];
        assert_eq!(
            completion_schema(&tables, Dialect::SqlServer),
            json!({
                "dbo.users": ["id", "email"],
                "loners": ["id"],
            })
        );
    }

    #[test]
    fn postgres_table_without_schema_stays_flat() {
        let tables = [table(None, "loners", &["id"])];
        assert_eq!(
            completion_schema(&tables, Dialect::Postgres),
            json!({ "loners": ["id"] })
        );
    }

    #[test]
    fn dots_in_identifiers_are_escaped_for_the_namespace() {
        // lang-sql splits namespace keys on unescaped dots, so a literal dot
        // in a schema or table name must arrive as `\.`.
        let tables = [table(
            Some("odd.schema"),
            "weird.table",
            &["a b", "sel.ect"],
        )];
        assert_eq!(
            completion_schema(&tables, Dialect::Postgres),
            json!({ r"odd\.schema.weird\.table": ["a b", "sel.ect"] })
        );
    }

    #[test]
    fn empty_schema_serializes_to_an_empty_object() {
        assert_eq!(completion_schema(&[], Dialect::Sqlite), json!({}));
    }
}
