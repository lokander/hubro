//! The SQL editor's query-history panel: search over the
//! connection's persisted history, per-entry Load / Run / Copy, clearing it,
//! and the recording opt-out.
//!
//! Its own module rather than part of [`super::editor`]: the panel is a
//! self-contained feature that touches the editor at exactly two points —
//! loading text into the buffer on screen, and asking [`AppState`] to run it.
//! Neither speaks to CodeMirror directly. Loading goes through
//! [`AppState::load_sql_text`], which moves the buffer's document target and
//! so makes the SQL pane push the text in; a `setDoc` here as well would be a
//! second way to do the same thing, and the one to fall behind.

use std::future::Future;

use chrono::{DateTime, Local};
use dioxus::prelude::*;

use crate::db::ConnectionId;
use crate::history::{HistoryEntry, HistoryStore, HISTORY_CAP};
use crate::ui::classes::{BTN_SECONDARY, BTN_SECONDARY_SM};

use super::js::write_clipboard;
use super::notice::{Banner, BannerKind, ConfirmInline, LoadingLine, SearchBox, SidePanel};
use super::state::AppState;

/// Cap on entries the history panel fetches per query. Matches the store's
/// retention cap so every persisted entry is reachable by scrolling, not only
/// via search.
const HISTORY_PANEL_LIMIT: i64 = HISTORY_CAP;

/// Right-side panel listing the current connection's persisted query
/// history: search, per-entry Load / Run / Copy, clear-history, and the
/// recording opt-out.
#[component]
pub(super) fn HistoryPanel(id: ConnectionId, buffer: u64) -> Element {
    let state = use_context::<AppState>();
    // The committed filter; the search box sets it on Enter, so each
    // keystroke doesn't hit the database.
    let search = use_signal(String::new);
    let mut confirm_clear = use_signal(|| false);
    let store_ready = state.history.read().is_some();
    let history_error = state.history_error.read().clone();
    let mut record_error_signal = state.history_record_error;
    let record_error = record_error_signal.read().clone();
    let recording = *state.history_recording.read();

    let entries = use_store_query(
        id,
        state.history_nonce,
        search,
        |store, locator, needle| async move {
            store
                .list(&locator, Some(&needle), HISTORY_PANEL_LIMIT)
                .await
        },
    );
    rsx! {
        SidePanel {
            header: rsx! {
                SearchBox { placeholder: "Search history (Enter)", search }
            },
            footer: rsx! {
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
                    ConfirmInline {
                        prompt: "Clear this connection's history?".to_string(),
                        confirm_label: "Clear",
                        on_confirm: move |_| {
                            state.clear_history(id);
                            confirm_clear.set(false);
                        },
                        on_cancel: move |_| confirm_clear.set(false),
                    }
                } else {
                    button {
                        class: "self-start {BTN_SECONDARY}",
                        disabled: !store_ready,
                        onclick: move |_| confirm_clear.set(true),
                        "Clear history"
                    }
                }
            },
            // A failed write for a completed run: warn without hiding
            // the still-readable list.
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
                // because HistoryRow needs owned props.
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
                                    buffer,
                                    entry: entry.clone(),
                                }
                            }
                        }
                    },
                }
            }
        }
    }
}

/// The list behind a history-store side panel (history, saved queries): runs
/// `query` with the store, the connection's locator and the committed search,
/// and again whenever the store opens, `nonce` is bumped or `search` changes.
/// Empty rather than an error while the store is not open yet or the
/// connection has no locator; the panels say "Opening…" for the first from
/// state of their own.
pub(super) fn use_store_query<T, Fut>(
    id: ConnectionId,
    nonce: Signal<u64>,
    search: Signal<String>,
    query: impl Fn(HistoryStore, String, String) -> Fut + 'static,
) -> Resource<Result<Vec<T>, String>>
where
    T: 'static,
    Fut: Future<Output = Result<Vec<T>, String>> + 'static,
{
    let state = use_context::<AppState>();
    use_resource(move || {
        // Reactive dependencies, all read before the await.
        let store = state.history.read().clone();
        let _refresh = *nonce.read();
        let needle = search.read().clone();
        let pending = match (store, state.connection_locator(id)) {
            (Some(store), Some(locator)) => Some(query(store, locator, needle)),
            _ => None,
        };
        async move {
            match pending {
                Some(pending) => pending.await,
                None => Ok(Vec::new()),
            }
        }
    })
}

/// One history entry: status dot, local-time stamp, one-line SQL preview
/// (full text in the tooltip), and Load / Run / Copy actions.
#[component]
fn HistoryRow(
    id: ConnectionId,
    /// The SQL buffer Load and Run target: the one on screen.
    buffer: u64,
    entry: HistoryEntry,
) -> Element {
    let state = use_context::<AppState>();
    let time = format_history_time(entry.executed_at);
    let preview: String = entry.sql.split_whitespace().collect::<Vec<_>>().join(" ");
    let sql_for_load = entry.sql.clone();
    let sql_for_run = entry.sql.clone();
    let sql_for_copy = entry.sql.clone();
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
                    class: BTN_SECONDARY_SM,
                    onclick: move |_| state.load_sql_text(id, buffer, sql_for_load.clone()),
                    "Load"
                }
                button {
                    class: "rounded border border-slate-300 dark:border-slate-700 px-1.5 py-0.5 text-xs text-cyan-700 dark:text-cyan-300 hover:bg-slate-200 dark:hover:bg-slate-800",
                    // Load into the editor buffer first so a write-confirm
                    // banner is confirmed against the text on screen.
                    onclick: move |_| {
                        state.load_sql_text(id, buffer, sql_for_run.clone());
                        state.run_sql(id, buffer, sql_for_run.clone());
                    },
                    "Run"
                }
                button {
                    class: BTN_SECONDARY_SM,
                    onclick: move |_| write_clipboard(&sql_for_copy),
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
