//! The SQL editor's query-history panel (FRE-23): search over the
//! connection's persisted history, per-entry Load / Run / Copy, clearing it,
//! and the recording opt-out.
//!
//! Its own module rather than part of [`super::editor`]: the panel is a
//! self-contained feature that touches the editor at exactly two points —
//! pushing text into the CodeMirror instance by element id, and asking
//! [`AppState`] to run it.

use chrono::{DateTime, Local};
use dioxus::prelude::*;

use crate::db::ConnectionId;
use crate::history::{HistoryEntry, HISTORY_CAP};

use super::js::{copy_to_clipboard, js_string};
use super::notice::{Banner, BannerKind, LoadingLine};
use super::state::AppState;

/// Cap on entries the history panel fetches per query. Matches the store's
/// retention cap so every persisted entry is reachable by scrolling, not only
/// via search (FRE-42).
const HISTORY_PANEL_LIMIT: i64 = HISTORY_CAP;

/// Right-side panel listing the current connection's persisted query
/// history: search, per-entry Load / Run / Copy, clear-history, and the
/// recording opt-out.
#[component]
pub(super) fn HistoryPanel(id: ConnectionId, editor_element: String) -> Element {
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
                    // Matched on the borrow, not a clone: up to HISTORY_CAP
                    // entries would otherwise be deep-copied on every render
                    // (each search keystroke). Only the rendered rows clone,
                    // because HistoryRow needs owned props (FRE-134).
                    match &*entries.read() {
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
                                for entry in entries.iter() {
                                    HistoryRow {
                                        key: "{entry.id}",
                                        id,
                                        editor_element: editor_element.clone(),
                                        entry: entry.clone(),
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
    let json_for_load = js_string(&entry.sql);
    let json_for_run = json_for_load.clone();
    let sql_for_load = entry.sql.clone();
    let sql_for_run = entry.sql.clone();
    let sql_for_copy = entry.sql.clone();
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
                    onclick: move |_| copy_to_clipboard(&sql_for_copy),
                    "Copy"
                }
            }
        }
    }
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
