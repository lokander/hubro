use std::path::PathBuf;

use dioxus::prelude::*;

use crate::db::ConnectionId;

use super::state::{ActiveView, AppState};

/// Top-level layout: tab bar over the active view.
#[component]
pub fn Shell() -> Element {
    let state = use_context::<AppState>();
    let active = *state.active.read();
    rsx! {
        div { class: "flex h-screen flex-col bg-slate-900 text-slate-100",
            TabBar {}
            main { class: "min-h-0 flex-1",
                match active {
                    ActiveView::Connections => rsx! { ConnectionsScreen {} },
                    ActiveView::Connection(id) => rsx! { ConnectionView { id } },
                }
            }
        }
    }
}

/// One tab per open connection, plus a fixed tab for the connections screen.
#[component]
fn TabBar() -> Element {
    let mut state = use_context::<AppState>();
    let active = *state.active.read();
    // Owned copies so the loop can hand ids/names to event handlers.
    let tabs: Vec<(ConnectionId, String)> = state
        .registry
        .read()
        .iter()
        .map(|c| (c.id, c.name.clone()))
        .collect();
    rsx! {
        header { class: "flex items-center gap-1 border-b border-slate-700 bg-slate-950 px-2 pt-1",
            button {
                class: if active == ActiveView::Connections {
                    "rounded-t px-3 py-1.5 text-sm bg-slate-900 text-slate-100"
                } else {
                    "rounded-t px-3 py-1.5 text-sm text-slate-400 hover:text-slate-100"
                },
                onclick: move |_| state.active.set(ActiveView::Connections),
                "Connections"
            }
            for (id, name) in tabs {
                div {
                    class: if active == ActiveView::Connection(id) {
                        "flex items-center gap-1 rounded-t bg-slate-900 px-3 py-1.5 text-sm text-slate-100"
                    } else {
                        "flex items-center gap-1 rounded-t px-3 py-1.5 text-sm text-slate-400 hover:text-slate-100"
                    },
                    button {
                        onclick: move |_| state.active.set(ActiveView::Connection(id)),
                        "{name}"
                    }
                    button {
                        class: "rounded px-1 text-slate-500 hover:bg-slate-700 hover:text-slate-200",
                        aria_label: "Close connection",
                        onclick: move |_| state.close_connection(id),
                        "×"
                    }
                }
            }
        }
    }
}

/// Layout for one open connection: schema sidebar left, content panel right.
/// The sidebar and grid are placeholders until FRE-8/FRE-9 land.
#[component]
fn ConnectionView(id: ConnectionId) -> Element {
    let state = use_context::<AppState>();
    let name = state.registry.read().get(id).map(|c| c.name.clone());
    let Some(name) = name else {
        // Tab was closed under us; the view switches away on the next render.
        return rsx! {
            div { class: "p-8 text-slate-400", "This connection is closed." }
        };
    };
    rsx! {
        div { class: "flex h-full",
            aside { class: "flex w-64 shrink-0 flex-col border-r border-slate-700 bg-slate-950/50",
                h2 { class: "border-b border-slate-800 px-4 py-3 font-mono text-sm text-slate-300",
                    "{name}"
                }
                p { class: "px-4 py-3 text-sm text-slate-500", "Schema browser coming soon." }
            }
            section { class: "flex min-w-0 flex-1 items-center justify-center",
                p { class: "text-slate-500", "Select a table to view its data." }
            }
        }
    }
}

/// Launch screen. The saved-connections list and native file picker arrive
/// with FRE-7; until then databases open via a path typed in directly.
#[component]
fn ConnectionsScreen() -> Element {
    let state = use_context::<AppState>();
    let mut path_input = use_signal(String::new);
    let error = state.connect_error.read().clone();

    let open = move || {
        let path = PathBuf::from(path_input.read().trim());
        if path.as_os_str().is_empty() {
            return;
        }
        spawn(async move { state.open_sqlite(path).await });
    };

    rsx! {
        div { class: "flex h-full flex-col items-center justify-center gap-4",
            h1 { class: "text-2xl font-semibold text-slate-200", "dataview" }
            p { class: "text-sm text-slate-400", "Open a SQLite database file to get started." }
            div { class: "flex w-full max-w-xl gap-2 px-8",
                input {
                    class: "min-w-0 flex-1 rounded border border-slate-700 bg-slate-950 px-3 py-2 font-mono text-sm text-slate-200 placeholder:text-slate-600",
                    placeholder: "/path/to/database.db",
                    value: "{path_input}",
                    oninput: move |evt| path_input.set(evt.value()),
                    onkeydown: move |evt| {
                        if evt.key() == Key::Enter {
                            open();
                        }
                    },
                }
                button {
                    class: "rounded bg-sky-600 px-4 py-2 text-sm font-medium text-white hover:bg-sky-500",
                    onclick: move |_| open(),
                    "Open"
                }
            }
            if let Some(err) = error {
                p { class: "max-w-xl px-8 text-sm text-red-400", "{err}" }
            }
        }
    }
}
