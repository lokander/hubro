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

/// Launch screen: the persisted saved-connections list. Add via the native
/// file picker; connect opens a tab (or focuses the existing one).
#[component]
fn ConnectionsScreen() -> Element {
    let state = use_context::<AppState>();
    let error = state.connect_error.read().clone();
    let saved: Vec<(String, PathBuf, bool)> = {
        let open_paths = state.open_paths.read();
        state
            .saved
            .read()
            .entries()
            .iter()
            .map(|s| {
                // open_paths holds canonicalized paths; compare like with like.
                let is_open = open_paths
                    .iter()
                    .any(|(_, p)| *p == super::state::canonical(&s.path));
                (s.name.clone(), s.path.clone(), is_open)
            })
            .collect()
    };

    let pick_file = move |_| {
        spawn(async move {
            let picked = rfd::AsyncFileDialog::new()
                .set_title("Add a SQLite database")
                .add_filter("SQLite databases", &["db", "sqlite", "sqlite3"])
                .add_filter("All files", &["*"])
                .pick_file()
                .await;
            if let Some(file) = picked {
                state.add_saved(file.path().to_path_buf());
            }
        });
    };

    rsx! {
        div { class: "flex h-full flex-col items-center justify-center gap-6",
            div { class: "text-center",
                h1 { class: "text-2xl font-semibold text-slate-200", "dataview" }
                p { class: "mt-1 text-sm text-slate-400",
                    if saved.is_empty() {
                        "Add a SQLite database file to get started."
                    } else {
                        "Pick a saved connection, or add another database."
                    }
                }
            }
            if !saved.is_empty() {
                ul { class: "w-full max-w-xl divide-y divide-slate-800 rounded border border-slate-700 bg-slate-950/60",
                    for (name, path, is_open) in saved {
                        li { class: "flex items-center gap-3 px-4 py-3",
                            button {
                                class: "min-w-0 flex-1 text-left",
                                onclick: {
                                    let path = path.clone();
                                    move |_| {
                                        let path = path.clone();
                                        spawn(async move { state.connect(path).await });
                                    }
                                },
                                div { class: "flex items-center gap-2",
                                    span { class: "truncate text-sm font-medium text-slate-200",
                                        "{name}"
                                    }
                                    if is_open {
                                        span { class: "rounded bg-sky-900/60 px-1.5 py-0.5 text-xs text-sky-300",
                                            "open"
                                        }
                                    }
                                }
                                div { class: "truncate font-mono text-xs text-slate-500",
                                    "{path.display()}"
                                }
                            }
                            button {
                                class: "rounded px-2 py-1 text-xs text-slate-500 hover:bg-slate-800 hover:text-slate-200",
                                aria_label: "Remove saved connection",
                                onclick: move |_| state.remove_saved(&path),
                                "Remove"
                            }
                        }
                    }
                }
            }
            button {
                class: "rounded bg-sky-600 px-4 py-2 text-sm font-medium text-white hover:bg-sky-500",
                onclick: pick_file,
                "Add database…"
            }
            if let Some(err) = error {
                p { class: "max-w-xl px-8 text-sm text-red-400", "{err}" }
            }
        }
    }
}
