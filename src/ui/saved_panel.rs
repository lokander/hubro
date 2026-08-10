//! The SQL editor's saved-queries panel (FRE-113): naming and keeping the
//! current buffer, browsing and searching what was kept, opening one into a
//! new query tab, and deleting.
//!
//! Its own module beside [`super::history_panel`], which it deliberately
//! mirrors — same width, same search idiom, same two-step destructive
//! action — because the two panels are the same shape of thing and only
//! differ in what they list.
//!
//! Opening never writes into the buffer on screen: it asks [`AppState`] for a
//! *new* one (see [`AppState::open_sql_buffer`]), which is the whole reason
//! opening a saved query is safe to click while half-way through writing
//! something else.

use dioxus::prelude::*;

use crate::db::ConnectionId;
use crate::history::SavedQuery;

use super::js::copy_to_clipboard;
use super::notice::{Banner, BannerKind, LoadingLine};
use super::state::AppState;

/// Right-side panel listing the queries saved for this connection plus the
/// global ones: save-current, search, and per-entry Open / Copy / Delete.
#[component]
pub(super) fn SavedQueriesPanel(
    id: ConnectionId,
    /// The buffer a save takes its text from — the one on screen.
    buffer: u64,
) -> Element {
    let state = use_context::<AppState>();
    // The controlled input; `search` only follows it on Enter (or when the
    // input is cleared), like the history panel's.
    let mut search_input = use_signal(String::new);
    let mut search = use_signal(String::new);
    let mut showing_form = use_signal(|| false);
    let store_ready = state.history.read().is_some();
    let history_error = state.history_error.read().clone();
    // Per connection (like the export status, FRE-73): another connection's
    // "Saved" line has no business showing up in this panel.
    let status = state.saved_status.read().get(&id).cloned();
    // Empty buffers are not worth naming, and the store refuses them anyway;
    // saying so on the button beats a red line after the fact.
    let buffer_empty = state.sql_buffer_text(id, buffer).trim().is_empty();

    let entries = use_resource(move || {
        // Reactive dependencies, all cloned out before the await.
        let store = state.history.read().clone();
        let _refresh = *state.saved_nonce.read();
        let needle = search.read().clone();
        let locator = state.connection_locator(id);
        async move {
            let (Some(store), Some(locator)) = (store, locator) else {
                return Ok(Vec::new());
            };
            store.saved_queries(&locator, Some(&needle)).await
        }
    });

    rsx! {
        aside { class: "flex w-80 shrink-0 flex-col border-l border-slate-300 dark:border-slate-700 bg-slate-50 dark:bg-slate-950/50",
            div { class: "flex flex-col gap-2 border-b border-slate-200 dark:border-slate-800 p-2",
                if showing_form() {
                    SaveForm {
                        id,
                        buffer,
                        on_close: move |_| showing_form.set(false),
                    }
                } else {
                    button {
                        class: "self-start rounded border border-slate-300 dark:border-slate-700 px-2 py-0.5 text-xs text-slate-500 dark:text-slate-400 hover:bg-slate-200 dark:hover:bg-slate-800 hover:text-slate-900 dark:hover:text-slate-100 disabled:opacity-50",
                        disabled: !store_ready || buffer_empty,
                        title: if buffer_empty {
                            "Write something in the editor first"
                        } else {
                            "Save what is in the editor under a name"
                        },
                        onclick: move |_| {
                            state.saved_status.clone().write().remove(&id);
                            showing_form.set(true);
                        },
                        "Save current query"
                    }
                }
                if let Some(status) = status.as_ref() {
                    {
                        let (text, class) = status.line();
                        rsx! { span { class: "text-xs {class}", "{text}" } }
                    }
                }
                input {
                    class: "w-full rounded border border-slate-300 dark:border-slate-700 bg-slate-100 dark:bg-slate-950 px-2 py-1 text-xs text-slate-900 dark:text-slate-200 placeholder:text-slate-400 dark:placeholder:text-slate-600",
                    placeholder: "Search saved queries (Enter)",
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
                if let Some(err) = history_error {
                    div { class: "p-2",
                        Banner {
                            kind: BannerKind::Warning,
                            message: format!("Saved queries are unavailable: {err}"),
                        }
                    }
                } else if !store_ready {
                    LoadingLine { label: "Opening saved queries…" }
                } else {
                    // Matched on the borrow, not a clone, for the same reason
                    // as the history list: only the rendered rows clone.
                    match &*entries.read() {
                        None => rsx! {
                            LoadingLine { label: "Loading…" }
                        },
                        Some(Err(err)) => rsx! {
                            div { class: "p-2",
                                Banner { kind: BannerKind::Warning, message: format!("Saved-query lookup failed: {err}") }
                            }
                        },
                        Some(Ok(entries)) if entries.is_empty() => rsx! {
                            p { class: "px-3 py-2 text-xs text-slate-500",
                                if search.read().trim().is_empty() {
                                    "Nothing saved yet. Write a query and press Save current query."
                                } else {
                                    "No matches."
                                }
                            }
                        },
                        Some(Ok(entries)) => rsx! {
                            ul { class: "divide-y divide-slate-200 dark:divide-slate-800/60",
                                for entry in entries.iter() {
                                    SavedRow { key: "{entry.id}", id, entry: entry.clone() }
                                }
                            }
                        },
                    }
                }
            }
        }
    }
}

/// The save form: a name, an optional description, and the global toggle.
/// Submitting hands the buffer's *current* text to the store, so what is
/// saved is what is on screen.
#[component]
fn SaveForm(id: ConnectionId, buffer: u64, on_close: EventHandler<()>) -> Element {
    let state = use_context::<AppState>();
    let mut name = use_signal(String::new);
    let mut description = use_signal(String::new);
    let mut global = use_signal(|| false);
    let submit = move || {
        let name_value = name.peek().trim().to_string();
        if name_value.is_empty() {
            return;
        }
        let description_value = description.peek().trim().to_string();
        state.save_query(
            id,
            buffer,
            name_value,
            (!description_value.is_empty()).then_some(description_value),
            state.sql_buffer_text(id, buffer),
            *global.peek(),
        );
        on_close.call(());
    };
    rsx! {
        div { class: "flex flex-col gap-1.5",
            input {
                class: "w-full rounded border border-slate-300 dark:border-slate-700 bg-slate-100 dark:bg-slate-950 px-2 py-1 text-xs text-slate-900 dark:text-slate-200 placeholder:text-slate-400 dark:placeholder:text-slate-600",
                placeholder: "Name",
                autofocus: true,
                value: "{name}",
                oninput: move |evt| name.set(evt.value()),
                onkeydown: move |evt: KeyboardEvent| {
                    if evt.key() == Key::Enter {
                        submit();
                    } else if evt.key() == Key::Escape {
                        on_close.call(());
                    }
                },
            }
            input {
                class: "w-full rounded border border-slate-300 dark:border-slate-700 bg-slate-100 dark:bg-slate-950 px-2 py-1 text-xs text-slate-900 dark:text-slate-200 placeholder:text-slate-400 dark:placeholder:text-slate-600",
                placeholder: "Description (optional)",
                value: "{description}",
                oninput: move |evt| description.set(evt.value()),
                onkeydown: move |evt: KeyboardEvent| {
                    if evt.key() == Key::Enter {
                        submit();
                    } else if evt.key() == Key::Escape {
                        on_close.call(());
                    }
                },
            }
            label { class: "flex items-center gap-2 text-xs text-slate-500 dark:text-slate-400",
                input {
                    r#type: "checkbox",
                    checked: global(),
                    onchange: move |evt| global.set(evt.checked()),
                }
                // Global is the exception, so it says what it costs: the
                // query shows up against databases it may not fit.
                span { title: "Offer this query on every connection, not just this one",
                    "Available on all connections"
                }
            }
            div { class: "flex gap-1",
                button {
                    class: "rounded bg-cyan-700 px-2 py-0.5 text-xs font-semibold text-white hover:bg-cyan-600 disabled:opacity-50",
                    disabled: name.read().trim().is_empty(),
                    onclick: move |_| submit(),
                    "Save"
                }
                button {
                    class: "rounded border border-slate-400 dark:border-slate-600 px-2 py-0.5 text-xs text-slate-900 dark:text-slate-300 hover:bg-slate-200 dark:hover:bg-slate-800",
                    onclick: move |_| on_close.call(()),
                    "Cancel"
                }
            }
        }
    }
}

/// One saved query: name, scope badge, description, a one-line SQL preview
/// (full text in the tooltip), and Open / Copy / Delete.
#[component]
fn SavedRow(id: ConnectionId, entry: SavedQuery) -> Element {
    let state = use_context::<AppState>();
    let mut confirm_delete = use_signal(|| false);
    let preview: String = entry.sql.split_whitespace().collect::<Vec<_>>().join(" ");
    let sql_for_open = entry.sql.clone();
    let sql_for_copy = entry.sql.clone();
    let name_for_open = entry.name.clone();
    let entry_id = entry.id;
    let global = entry.locator.is_none();
    let title = match &entry.description {
        Some(description) => format!("{description}\n\n{}", entry.sql),
        None => entry.sql.clone(),
    };
    rsx! {
        li { class: "px-3 py-2", title: "{title}",
            div { class: "flex items-center gap-2",
                span { class: "min-w-0 flex-1 truncate text-xs font-semibold text-slate-900 dark:text-slate-200",
                    "{entry.name}"
                }
                if global {
                    span {
                        class: "shrink-0 rounded bg-slate-200 dark:bg-slate-800 px-1 py-0.5 text-[10px] leading-none text-slate-600 dark:text-slate-400",
                        title: "Saved for every connection",
                        "global"
                    }
                }
            }
            if let Some(description) = entry.description.as_ref() {
                p { class: "mt-0.5 truncate text-xs text-slate-500 dark:text-slate-400", "{description}" }
            }
            p { class: "mt-0.5 truncate font-mono text-xs text-slate-500 dark:text-slate-500", "{preview}" }
            div { class: "mt-1 flex gap-1",
                button {
                    class: "rounded border border-slate-300 dark:border-slate-700 px-1.5 py-0.5 text-xs text-cyan-700 dark:text-cyan-300 hover:bg-slate-200 dark:hover:bg-slate-800",
                    title: "Open in a new query tab",
                    onclick: move |_| {
                        state.open_sql_buffer(id, Some(name_for_open.clone()), sql_for_open.clone());
                    },
                    "Open"
                }
                button {
                    class: "rounded border border-slate-300 dark:border-slate-700 px-1.5 py-0.5 text-xs text-slate-500 dark:text-slate-400 hover:bg-slate-200 dark:hover:bg-slate-800 hover:text-slate-900 dark:hover:text-slate-100",
                    onclick: move |_| copy_to_clipboard(&sql_for_copy),
                    "Copy"
                }
                if confirm_delete() {
                    button {
                        class: "rounded bg-rose-600 px-1.5 py-0.5 text-xs font-semibold text-white hover:bg-rose-500",
                        onclick: move |_| {
                            state.delete_saved_query(id, entry_id);
                            confirm_delete.set(false);
                        },
                        "Delete for good"
                    }
                    button {
                        class: "rounded border border-slate-400 dark:border-slate-600 px-1.5 py-0.5 text-xs text-slate-900 dark:text-slate-300 hover:bg-slate-200 dark:hover:bg-slate-800",
                        onclick: move |_| confirm_delete.set(false),
                        "Keep"
                    }
                } else {
                    button {
                        class: "rounded border border-slate-300 dark:border-slate-700 px-1.5 py-0.5 text-xs text-slate-500 dark:text-slate-400 hover:bg-slate-200 dark:hover:bg-slate-800 hover:text-slate-900 dark:hover:text-slate-100",
                        onclick: move |_| confirm_delete.set(true),
                        "Delete"
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::state::SavedStatus;

    /// The status line the panel renders is the state layer's
    /// [`SavedStatus::line`]; this only pins that the panel keeps using it,
    /// so a save and a failure can never render the same way.
    #[test]
    fn a_save_and_a_failure_read_differently() {
        let saved = SavedStatus::Saved {
            name: "Counts".into(),
            replaced: false,
        };
        let failed = SavedStatus::Failed("no room".into());
        assert_ne!(saved.line().0, failed.line().0);
        assert_ne!(saved.line().1, failed.line().1);
    }
}
