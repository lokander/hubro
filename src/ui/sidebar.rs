use dioxus::prelude::*;
use dioxus_icons::lucide::{Database, RefreshCw, Search, X};

use crate::db::{ConnectionId, TableKind, TableMeta};

use super::filter::{filter_tables, group_by_schema, parse_query, toggle_column_mode, FilterMode};
use super::notice::{Banner, BannerKind, DelayedLoading, EmptyState};
use super::state::{AppState, SchemaLoad, TableRef};

/// Puts the caret back in the filter box after one of the buttons inside it
/// was clicked. Without this the button keeps focus and the next keystroke
/// goes nowhere, which is exactly what you type after flipping to `:col`.
///
/// Deferred to the next frame so it runs after the re-render that applies the
/// new text — otherwise the caret is placed against the old value. Same trick
/// as the cell editor's focus fallback in `editing.rs`.
fn focus_filter_input() {
    document::eval(
        "requestAnimationFrame(() => { \
            const el = document.getElementById('dv-schema-filter'); \
            if (el) { el.focus(); el.setSelectionRange(el.value.length, el.value.length); } \
        });",
    );
}

/// Sidebar for one connection: a flat list of the introspected tables and
/// views, grouped by schema. Clicking a name selects it for the data grid
/// and the Schema pane, which is where its columns and indexes live
/// (FRE-69).
///
/// A filter box sits above the list (FRE-107). It is purely client-side over
/// the schema already in `AppState` — no query is ever issued — and it stays
/// in this component's local state rather than `TabUi`, so it is not
/// persisted to the session and a tab reopened later starts unfiltered.
#[component]
pub fn SchemaSidebar(id: ConnectionId) -> Element {
    let state = use_context::<AppState>();
    let mut filter = use_signal(String::new);
    let schema = state
        .schemas
        .read()
        .get(&id)
        .cloned()
        .unwrap_or(SchemaLoad::Loading);

    let raw = filter();
    let query = parse_query(&raw);
    let columns_mode = query.mode == FilterMode::Columns;
    // The box only helps once there is a list to narrow, and it would be a
    // confusing thing to offer next to a load error.
    let show_filter = matches!(&schema, SchemaLoad::Ready(tables) if !tables.is_empty());

    rsx! {
        div { class: "flex min-h-0 flex-1 flex-col",
            div { class: "flex items-center justify-between border-b border-slate-200 dark:border-slate-800 px-4 py-2",
                span { class: "text-xs font-semibold uppercase tracking-wide text-slate-500",
                    "Schema"
                }
                button {
                    class: "flex items-center gap-1 rounded px-2 py-0.5 text-xs text-slate-500 dark:text-slate-400 hover:bg-slate-200 dark:hover:bg-slate-800 hover:text-slate-900 dark:hover:text-slate-100",
                    title: "Reload schema",
                    onclick: move |_| state.load_schema(id),
                    RefreshCw { size: 12 }
                    "Reload"
                }
            }
            // Pinned above the scroll area, so it stays put while the list
            // under it scrolls.
            if show_filter {
                div { class: "border-b border-slate-200 dark:border-slate-800 px-2 py-1.5",
                    div { class: "flex items-center gap-1 rounded border border-slate-300 dark:border-slate-700 bg-slate-100 dark:bg-slate-950 px-2 py-1 focus-within:border-sky-500 dark:focus-within:border-sky-600",
                        span { class: "shrink-0 text-slate-400 dark:text-slate-600",
                            Search { size: 12 }
                        }
                        input {
                            // `dv-schema-filter` is the target of the Ctrl+F
                            // focus shortcut (FRE-107); the grid's own filter
                            // keeps `/` and `dv-filter`.
                            id: "dv-schema-filter",
                            class: "min-w-0 flex-1 bg-transparent font-mono text-xs text-slate-900 dark:text-slate-200 placeholder:text-slate-400 dark:placeholder:text-slate-600 focus:outline-none",
                            placeholder: if columns_mode { "column…" } else { "table…" },
                            title: "Filter the list (Ctrl+F). Esc clears; “:col name” searches column names.",
                            value: "{raw}",
                            oninput: move |evt| filter.set(evt.value()),
                            // Handled here rather than in the window listener,
                            // which deliberately ignores keys typed into an
                            // input (see GLOBAL_KEYS_JS in shell.rs).
                            onkeydown: move |evt: KeyboardEvent| {
                                if evt.key() == Key::Escape {
                                    filter.set(String::new());
                                }
                            },
                        }
                        // Writes the prefix into the box rather than holding a
                        // separate mode flag, so the toggle and a typed
                        // ":col" can never disagree.
                        button {
                            class: if columns_mode {
                                "shrink-0 rounded bg-violet-100 dark:bg-violet-900/50 px-1 font-mono text-xs text-violet-700 dark:text-violet-300"
                            } else {
                                "shrink-0 rounded px-1 font-mono text-xs text-slate-400 dark:text-slate-600 hover:bg-slate-200 dark:hover:bg-slate-800 hover:text-slate-900 dark:hover:text-slate-100"
                            },
                            title: "Search column names instead of table names",
                            aria_label: "Search column names",
                            onclick: move |_| {
                                let toggled = toggle_column_mode(&filter.peek());
                                filter.set(toggled);
                                focus_filter_input();
                            },
                            ":col"
                        }
                        if !raw.is_empty() {
                            button {
                                class: "shrink-0 rounded text-slate-400 dark:text-slate-600 hover:text-slate-900 dark:hover:text-slate-200",
                                title: "Clear the filter (Esc)",
                                aria_label: "Clear the filter",
                                onclick: move |_| {
                                    filter.set(String::new());
                                    focus_filter_input();
                                },
                                X { size: 12 }
                            }
                        }
                    }
                }
            }
            div { class: "min-h-0 flex-1 overflow-y-auto py-1",
                match schema {
                    SchemaLoad::Loading => rsx! {
                        DelayedLoading { label: "Loading schema…" }
                    },
                    SchemaLoad::Failed(err) => rsx! {
                        div { class: "p-2",
                            Banner { kind: BannerKind::Error, message: err }
                        }
                    },
                    SchemaLoad::Ready(tables) if tables.is_empty() => rsx! {
                        EmptyState {
                            icon: rsx! { Database { size: 40 } },
                            title: "No tables",
                            hint: "This database has no tables yet.",
                        }
                    },
                    SchemaLoad::Ready(tables) => {
                        // An empty needle matches everything in introspection
                        // order, so the unfiltered tree renders through this
                        // same path — there is no separate "no filter" branch
                        // to keep in sync.
                        let hits = filter_tables(&tables, &query);
                        let groups = group_by_schema(&tables, &hits);
                        // Schemas that contributed no hit contribute no header
                        // either, which is what keeps the result reading as a
                        // narrowed tree rather than a flat list of names.
                        let show_headers = groups.len() > 1
                            || groups.first().is_some_and(|(s, _)| s.is_some());
                        rsx! {
                            if hits.is_empty() {
                                p { class: "px-3 py-4 text-center text-xs text-slate-500 dark:text-slate-400",
                                    if columns_mode { "No columns match " } else { "No tables match " }
                                    span { class: "font-mono text-slate-700 dark:text-slate-300", "{query.needle}" }
                                }
                            }
                            for (schema, group) in groups {
                                if show_headers {
                                    p { class: "px-2 pt-2 pb-0.5 text-xs font-semibold uppercase tracking-wide text-slate-400 dark:text-slate-600",
                                        {schema.clone().unwrap_or_else(|| "(no schema)".to_string())}
                                    }
                                }
                                ul {
                                    for hit in group {
                                        TableNode {
                                            key: "{tables[hit.index].schema:?}.{tables[hit.index].name}",
                                            id,
                                            table: tables[hit.index].clone(),
                                            columns: hit.columns.clone(),
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

/// One table/view in the list: its name plus a kind badge. Structure lives
/// in the Schema pane now (FRE-69), so there is nothing to expand — except in
/// the filter's column mode, where `columns` carries the matching columns to
/// list underneath (FRE-107).
#[component]
fn TableNode(
    id: ConnectionId,
    table: ReadSignal<TableMeta>,
    /// Positions in `table.columns` that matched a column-mode filter, in
    /// declaration order. Empty in the default mode.
    #[props(default)]
    columns: Vec<usize>,
) -> Element {
    let state = use_context::<AppState>();
    let name = table.read().name.clone();
    let kind = table.read().kind;
    // Resolved here so the loop below doesn't hold a read borrow of the
    // signal while rendering.
    let matched: Vec<(String, String)> = columns
        .iter()
        .filter_map(|position| {
            table
                .read()
                .columns
                .get(*position)
                .map(|column| (column.name.clone(), column.type_name.clone()))
        })
        .collect();
    let table_ref = TableRef {
        schema: table.read().schema.clone(),
        name: name.clone(),
    };
    let selected = state
        .tab_ui
        .read()
        .get(&id)
        .is_some_and(|ui| ui.selected_table.as_ref() == Some(&table_ref));

    let select_ref = table_ref.clone();
    let select_from_column = table_ref.clone();
    rsx! {
        li {
            div {
                class: if selected {
                    "flex items-center gap-1 bg-sky-100 dark:bg-sky-900/40 px-2 py-1 text-sm text-sky-700 dark:text-sky-200"
                } else {
                    "flex items-center gap-1 px-2 py-1 text-sm text-slate-900 dark:text-slate-300 hover:bg-slate-200 dark:hover:bg-slate-800/60"
                },
                button {
                    // `dv-table-btn` + `data-selected` drive the keyboard
                    // sidebar navigation (FRE-15): the global key listener
                    // focuses these buttons (Ctrl+B) and moves focus between
                    // them with ↑/↓; a focused button opens its table on
                    // Enter/Space natively via this onclick. It reads the DOM,
                    // so it arrows through the filtered list for free.
                    class: "dv-table-btn flex min-w-0 flex-1 items-center gap-2 text-left",
                    "data-selected": selected,
                    onclick: move |_| state.select_table(id, &select_ref),
                    span { class: "truncate font-mono", "{name}" }
                    if kind == TableKind::View {
                        span { class: "rounded bg-violet-100 dark:bg-violet-900/50 px-1 text-xs text-violet-700 dark:text-violet-300",
                            "view"
                        }
                    }
                    if kind == TableKind::MaterializedView {
                        span { class: "rounded bg-fuchsia-100 dark:bg-fuchsia-900/50 px-1 text-xs text-fuchsia-700 dark:text-fuchsia-300",
                            "matview"
                        }
                    }
                }
            }
            // Column-mode hits, indented under the table that owns them.
            // Clicking one selects that table — the sidebar's only navigation
            // — but they are deliberately not `dv-table-btn`, so Ctrl+B and
            // the ↑/↓ nav keep stepping table to table.
            if !matched.is_empty() {
                ul { class: "mb-1",
                    for (column_name , type_name) in matched {
                        li {
                            button {
                                class: "flex w-full min-w-0 items-baseline gap-2 py-0.5 pl-7 pr-2 text-left text-xs hover:bg-slate-200 dark:hover:bg-slate-800/60",
                                onclick: {
                                    let table_ref = select_from_column.clone();
                                    move |_| state.select_table(id, &table_ref)
                                },
                                span { class: "truncate font-mono text-slate-700 dark:text-slate-400",
                                    "{column_name}"
                                }
                                span { class: "shrink-0 font-mono text-slate-400 dark:text-slate-600",
                                    "{type_name}"
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}
