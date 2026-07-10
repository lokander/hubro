use dioxus::prelude::*;
use serde::Deserialize;

use crate::db::{
    classify_statement, ConnectionId, Dialect, QueryResult, StatementKind, StatementOutcome,
    StatementResult, Value,
};

use super::state::{AppState, RunStatus, CANCELLED};

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
        let dialect_name = match dialect {
            Dialect::Postgres => "postgres",
            Dialect::Sqlite => "sqlite",
        };
        let initial_json = serde_json::to_string(&initial).unwrap_or_else(|_| "\"\"".into());
        spawn(async move {
            let js = format!(
                r#"
                window.__dvRun = (p) => dioxus.send(JSON.stringify({{ kind: "run", sql: p.sql }}));
                window.__dvDoc = (p) => dioxus.send(JSON.stringify({{ kind: "doc", doc: p.doc }}));
                DVEditor.create("{element}", "{element}", "{dialect_name}", {initial_json});
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

    let element_for_drop = editor_element.clone();
    use_drop(move || {
        document::eval(&format!(r#"DVEditor.destroy("{element_for_drop}");"#));
    });

    let run = state.sql_runs.read().get(&id).cloned();
    let running = matches!(run.as_ref().map(|r| &r.status), Some(RunStatus::Running));
    let pending_writes = state.pending_sql.read().get(&id).map(|statements| {
        statements
            .iter()
            .filter(|s| classify_statement(s) == StatementKind::Write)
            .count()
    });

    rsx! {
        div { class: "flex h-full min-h-0 flex-col",
            div { class: "flex items-center justify-between border-b border-slate-800 px-3 py-1.5",
                span { class: "text-xs text-slate-500",
                    "Ctrl+Enter runs the buffer — or just the selection."
                }
                if running {
                    button {
                        class: "rounded border border-rose-800 px-2 py-0.5 text-xs text-rose-300 hover:bg-rose-950/50",
                        onclick: move |_| state.cancel_sql(id),
                        "Cancel"
                    }
                }
            }
            div {
                id: "{editor_element}",
                class: "h-1/2 min-h-0 shrink-0 overflow-hidden border-b border-slate-700 text-sm",
            }
            div { class: "min-h-0 flex-1 overflow-auto",
                if let Some(write_count) = pending_writes {
                    div { class: "flex items-center gap-3 border-b border-amber-900/50 bg-amber-950/30 px-4 py-2",
                        span { class: "text-sm text-amber-300",
                            if write_count == 1 {
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
                            class: "rounded border border-slate-600 px-2.5 py-0.5 text-xs text-slate-300 hover:bg-slate-800",
                            onclick: move |_| state.dismiss_pending_sql(id),
                            "Cancel"
                        }
                    }
                }
                match run {
                    None => rsx! {
                        p { class: "px-4 py-3 text-sm text-slate-500", "Results appear here." }
                    },
                    Some(run) => rsx! {
                        for (index, statement) in run.statements.iter().enumerate() {
                            StatementSection {
                                key: "{index}",
                                index: index + 1,
                                result: statement.clone(),
                            }
                        }
                        RunStatusLine { status: run.status.clone(), statement_count: run.statements.len() }
                    },
                }
            }
        }
    }
}

/// The script-level footer: running indicator, total elapsed time (shown
/// once per run), or the failure block.
#[component]
fn RunStatusLine(status: RunStatus, statement_count: usize) -> Element {
    match status {
        RunStatus::Running => rsx! {
            p { class: "px-4 py-3 text-sm text-slate-500", "Running…" }
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
        } => {
            if error == CANCELLED {
                rsx! {
                    p { class: "border-t border-amber-900/50 px-4 py-3 text-sm text-amber-300",
                        "Run cancelled."
                    }
                }
            } else {
                rsx! {
                    div { class: "border-t border-red-900/50 bg-red-950/20 px-4 py-3",
                        p { class: "mb-1 font-mono text-xs text-red-300/80",
                            "{statement_index + 1} · {preview}"
                        }
                        p { class: "font-mono text-sm text-red-400", "{error}" }
                        p { class: "mt-1 text-xs text-slate-500",
                            "Script stopped after {elapsed_ms} ms; earlier statements were not rolled back."
                        }
                    }
                }
            }
        }
    }
}

/// One executed statement's section: a header naming the statement plus its
/// row count or affected count, and the result table for reads.
#[component]
fn StatementSection(index: usize, result: StatementResult) -> Element {
    let summary = match &result.outcome {
        StatementOutcome::Affected(1) => "1 row affected".to_string(),
        StatementOutcome::Affected(n) => format!("{n} rows affected"),
        StatementOutcome::Rows(r) if r.rows.len() == 1 => "1 row".to_string(),
        StatementOutcome::Rows(r) => format!("{} rows", r.rows.len()),
    };
    rsx! {
        div { class: "border-b border-slate-800",
            p { class: "flex items-baseline gap-2 bg-slate-900/60 px-4 py-1.5 text-xs",
                span { class: "font-mono text-slate-500", "{index}" }
                span { class: "min-w-0 truncate font-mono text-slate-300", "{result.preview}" }
                span { class: "shrink-0 text-cyan-400", "— {summary}" }
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

/// A result grid, capped at [`MAX_RENDERED_ROWS`] rendered rows.
#[component]
fn ResultTable(result: QueryResult) -> Element {
    rsx! {
        if result.rows.len() > MAX_RENDERED_ROWS {
            p { class: "border-b border-amber-900/50 bg-amber-950/30 px-4 py-1.5 text-xs text-amber-300",
                "Showing the first {MAX_RENDERED_ROWS} of {result.rows.len()} rows."
            }
        }
        table { class: "w-full border-collapse text-left",
            thead { class: "sticky top-0 bg-slate-900",
                tr {
                    for column in result.columns.iter() {
                        th { class: "border-b border-slate-700 px-3 py-1.5 font-mono text-xs font-semibold text-slate-300",
                            "{column.name}"
                        }
                    }
                }
            }
            tbody {
                for row in result.rows.iter().take(MAX_RENDERED_ROWS) {
                    tr { class: "border-t border-slate-800/60 hover:bg-slate-800/30",
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
        Value::Null => "px-3 py-1 font-mono text-xs italic text-slate-600",
        Value::Blob(_) => "px-3 py-1 font-mono text-xs text-violet-400",
        _ => "px-3 py-1 font-mono text-xs text-slate-200",
    };
    rsx! {
        td { class,
            div { class: "max-w-md truncate", title: "{display}", "{display}" }
        }
    }
}
