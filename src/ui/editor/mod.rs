//! The SQL pane: a strip of query tabs over the CodeMirror editor, with the
//! results of the active buffer's last run below.
//!
//! - [`surface`] — the CodeMirror host and its JavaScript;
//! - [`results`] — the run's statement sections, result tables and status;
//! - [`plan`] — an Explain's plan.
//!
//! The submodules open with `use super::*`, inheriting this file's imports:
//! they are one component tree split across files.

mod plan;
mod results;
mod surface;

use plan::*;
use results::*;
use surface::*;

use dioxus::prelude::*;
use dioxus_icons::lucide::{Play, Plus, X};
use serde::Deserialize;
use serde_json::json;

use crate::db::{
    needs_confirmation, plan_reached_cell_cap, ConnectionId, Dialect, ExportFormat, LateRest,
    OpenTransaction, PlanDisplay, PlanNode, PlanTree, RestOfRead, Rollback, RowsAffected, SetOf,
    StatementOutcome, TableMeta, Value, EXPENSIVE_SHARE, MARKED_READ_ONLY, MAX_QUERY_ROWS,
    MAX_RESULT_SETS, NO_DDL, NO_EXPLAIN, NO_MUTATE, NO_QUERY, PLAN_CELL_CAP,
};
use crate::ui::classes::{BTN_SECONDARY, BTN_SECONDARY_SM};

use super::history_panel::HistoryPanel;
use super::js::js_string;
use super::notice::{Banner, BannerKind, EmptyState, LoadingLine};
use super::pick_export_path;
use super::saved_panel::SavedQueriesPanel;
use super::state::{
    orphaned_sql_export_status, sql_export_status, AppState, ExportStatus, RunStatus, SchemaLoad,
    SharedStatement, SqlBuffer,
};

/// Which side panel the SQL pane is showing. At most one at a time: they are
/// the same width and the same shape, and both open would leave no editor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EditorPanel {
    None,
    History,
    Saved,
}

/// The name on a query tab: the saved query it was opened from, or its
/// position for a scratch buffer. Position rather than "Untitled"
/// so several scratch buffers stay tellable apart.
fn buffer_label(buffer: &SqlBuffer, index: usize) -> String {
    match &buffer.title {
        Some(title) => title.clone(),
        None => format!("Query {}", index + 1),
    }
}

/// Free-form SQL pane: a strip of query tabs over a CodeMirror 6 editor
/// (bundled locally in assets/codemirror.js — the app works offline), results
/// below. Ctrl+Enter runs the buffer, or just the selection when one exists.
#[component]
pub fn SqlEditor(id: ConnectionId) -> Element {
    let state = use_context::<AppState>();
    // `None` once the connection is gone (the tab is closing). The editor is
    // then not mounted at all: its grammar and completions are the dialect's,
    // and it would only hand Ctrl+Enter to a connection that is not there.
    let dialect: Option<Dialect> = state.registry.read().get(id).map(|c| c.pool.dialect());
    // Whether this connection can produce a plan at all, and whether that
    // plan will be structured. `None` disables the Explain button
    // rather than hiding it: "SQL Server has no EXPLAIN" is worth saying once
    // more than it is worth leaving the user to wonder.
    let explain_support = state
        .registry
        .read()
        .get(id)
        .and_then(|c| c.pool.explain_support());
    let structured_plans = explain_support.is_some_and(|support| support.structured);
    let editor_element = editor_element_id(id);
    // What the editor should be showing: the active buffer *and* the
    // generation of the last text put into a buffer from outside the editor.
    // The buffer id alone is not enough — opening a saved query into an
    // untitled blank buffer reuses it, so the id does not change, and an
    // id-gated effect would rename the tab while leaving the editor empty.
    let doc_target = use_memo(move || state.sql_doc_target(id));
    let active = doc_target().0;

    // Show the active buffer's text. Runs on a real switch or open only —
    // the memo gates it — and reads the text with `peek`, so a keystroke
    // never pushes the document back into the editor under the caret.
    //
    // The target goes to the bundle as well as the text: `setDoc` flushes
    // whatever the outgoing document still owed under its *old* token before
    // taking the new one, which is what keeps the last 250 ms of
    // typing from vanishing into the buffer being switched away from.
    let element_for_switch = editor_element.clone();
    use_effect(move || {
        let (buffer, generation) = doc_target();
        let text = state
            .tab_ui
            .peek()
            .get(&id)
            .map_or(String::new(), |ui| ui.sql.text(buffer).to_string());
        document::eval(&format!(
            r#"DVEditor.setDoc("{element_for_switch}", {}, {buffer}, {generation});"#,
            js_string(&text)
        ));
    });

    // Whether the active buffer's run is in flight, for the Cancel button.
    // Reading the run subscribes this pane to its progress; the results
    // themselves are [`RunResults`]'s to draw.
    let running = matches!(
        state.sql_runs.read().get(&(id, active)).map(|r| &r.status),
        Some(RunStatus::Running)
    );
    // What this connection won't run, stated before the user writes it.
    // `run_sql` refuses the same cases, so the note explains a refusal that
    // would otherwise arrive only after pressing Ctrl+Enter.
    //
    // Effective capabilities, so a connection the user marked read-only
    // says so here rather than only when the run is refused.
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
    let panel = use_signal(|| EditorPanel::None);

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
                            onclick: move |_| state.cancel_sql(id, active),
                            "Cancel"
                        }
                    }
                    button {
                        class: if explain_support.is_some() {
                            BTN_SECONDARY
                        } else {
                            "rounded border border-slate-200 dark:border-slate-800 px-2 py-0.5 text-xs text-slate-400 dark:text-slate-600 cursor-not-allowed"
                        },
                        disabled: explain_support.is_none(),
                        title: match explain_support {
                            // Named rather than implied: EXPLAIN shows the
                            // plan without running the statement, but an
                            // EXPLAIN ANALYZE the user wrote themselves does
                            // run it — and is passed through as written.
                            Some(_) => "Show the query plan for this buffer, without running it",
                            None => NO_EXPLAIN,
                        },
                        onclick: move |_| state.run_explain(id, active),
                        "Explain"
                    }
                    PanelButton { panel, target: EditorPanel::Saved, label: "Saved" }
                    PanelButton { panel, target: EditorPanel::History, label: "History" }
                }
            }
            // The query tabs. Opening a saved query adds one here instead of
            // replacing what is in the editor.
            QueryTabs { id, active }
            if let Some(dialect) = dialect {
                EditorSurface { id, dialect }
            } else {
                p { class: "border-b border-slate-300 dark:border-slate-700 px-4 py-3 text-sm text-slate-500",
                    "This connection is closing, so its SQL editor is not available."
                }
            }
            RunResults { id, active, dialect, structured_plans }
        }
        match panel() {
            EditorPanel::None => rsx! {},
            EditorPanel::History => rsx! {
                HistoryPanel { id, buffer: active }
            },
            EditorPanel::Saved => rsx! {
                SavedQueriesPanel { id, buffer: active }
            },
        }
        }
    }
}

/// One of the SQL toolbar's panel toggles. Clicking the open one closes it.
#[component]
fn PanelButton(panel: Signal<EditorPanel>, target: EditorPanel, label: String) -> Element {
    let mut panel = panel;
    rsx! {
        button {
            class: if panel() == target {
                "rounded bg-slate-300 dark:bg-slate-700 px-2 py-0.5 text-xs text-slate-900 dark:text-slate-100"
            } else {
                BTN_SECONDARY
            },
            onclick: move |_| {
                let showing = panel() == target;
                panel.set(if showing { EditorPanel::None } else { target });
            },
            "{label}"
        }
    }
}

/// The strip of query tabs over the editor, with the new-tab button.
#[component]
fn QueryTabs(
    id: ConnectionId,
    /// The buffer the editor is showing.
    active: u64,
) -> Element {
    let state = use_context::<AppState>();
    // A memo: it reads `tab_ui`, which changes on every keystroke, and a
    // memo's PartialEq gate is what keeps the strip from re-rendering for
    // text nobody's tab strip can see. (The active buffer is a memo too, in
    // [`SqlEditor`], for the same reason.)
    let tabs = use_memo(move || {
        state
            .sql_buffers(id)
            .iter()
            .enumerate()
            .map(|(index, buffer)| (buffer.id, buffer_label(buffer, index)))
            .collect::<Vec<(u64, String)>>()
    });
    rsx! {
        div { class: "flex items-center gap-1 overflow-x-auto border-b border-slate-200 dark:border-slate-800 px-2 py-1",
            for (buffer_id, label) in tabs().into_iter() {
                div {
                    key: "{buffer_id}",
                    class: if buffer_id == active {
                        "flex shrink-0 items-center gap-1 rounded bg-slate-200 dark:bg-slate-700 px-2 py-0.5 text-xs text-slate-900 dark:text-slate-100"
                    } else {
                        "flex shrink-0 items-center gap-1 rounded px-2 py-0.5 text-xs text-slate-500 dark:text-slate-400 hover:bg-slate-100 dark:hover:bg-slate-800"
                    },
                    button {
                        class: "max-w-40 truncate",
                        title: "{label}",
                        onclick: move |_| state.select_sql_buffer(id, buffer_id),
                        "{label}"
                    }
                    if tabs.read().len() > 1 {
                        button {
                            class: "rounded px-0.5 text-slate-500 hover:bg-slate-300 dark:hover:bg-slate-600 hover:text-slate-900 dark:hover:text-slate-100",
                            aria_label: "Close query tab",
                            onclick: move |_| state.close_sql_buffer(id, buffer_id),
                            X { size: 10 }
                        }
                    }
                }
            }
            button {
                class: "shrink-0 rounded px-1.5 py-0.5 text-xs text-slate-500 dark:text-slate-400 hover:bg-slate-100 dark:hover:bg-slate-800 hover:text-slate-900 dark:hover:text-slate-100",
                title: "New query tab",
                aria_label: "New query tab",
                onclick: move |_| {
                    state.new_sql_buffer(id);
                },
                Plus { size: 12 }
            }
        }
    }
}
