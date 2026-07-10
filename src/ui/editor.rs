use dioxus::prelude::*;
use serde::Deserialize;

use crate::db::{ConnectionId, Dialect, Value};

use super::state::{AppState, SqlRun};

/// Cap on rendered result rows. The full result still sits in memory until
/// FRE-33 introduces streaming/limits at the query layer, but the DOM must
/// not receive a million rows.
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

    rsx! {
        div { class: "flex h-full min-h-0 flex-col",
            div { class: "flex items-center justify-between border-b border-slate-800 px-3 py-1.5",
                span { class: "text-xs text-slate-500",
                    "Ctrl+Enter runs the buffer — or just the selection."
                }
            }
            div {
                id: "{editor_element}",
                class: "h-1/2 min-h-0 shrink-0 overflow-hidden border-b border-slate-700 text-sm",
            }
            div { class: "min-h-0 flex-1 overflow-auto",
                match run {
                    None => rsx! {
                        p { class: "px-4 py-3 text-sm text-slate-500", "Results appear here." }
                    },
                    Some(SqlRun::Running) => rsx! {
                        p { class: "px-4 py-3 text-sm text-slate-500", "Running…" }
                    },
                    Some(SqlRun::Failed(err)) => rsx! {
                        p { class: "px-4 py-3 font-mono text-sm text-red-400", "{err}" }
                    },
                    Some(SqlRun::Done(result)) => rsx! {
                        if result.rows.is_empty() {
                            p { class: "px-4 py-3 text-sm text-slate-500",
                                "The statement returned no rows."
                            }
                        } else {
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
                    },
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
