use dioxus::prelude::*;
use dioxus_icons::lucide::{Database, RefreshCw, Search, X};

use crate::db::{ConnectionId, TableKind, TableMeta};

use super::filter::{
    count_internal_tables, filter_tables, group_by_schema, parse_query, toggle_column_mode,
    FilterMode,
};
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

/// What the sidebar's body should show. Derived from [`SchemaLoad`] so the
/// render body never has to clone the table list just to find out which
/// branch it is in — on a 300-table schema that clone alone cost ~1 ms per
/// render (FRE-107). The list itself comes from the memo below.
#[derive(Clone, PartialEq)]
enum ListState {
    Loading,
    Failed(String),
    /// Introspected fine, but the database has no tables at all.
    Empty,
    Ready,
}

/// One schema group of filter results, ready to render: the schema name and
/// its surviving tables, each with the positions of its matching columns
/// (empty outside column mode).
type FilteredGroups = Vec<(Option<String>, Vec<(TableMeta, Vec<usize>)>)>;

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
    let list_state = match state.schemas.read().get(&id) {
        Some(SchemaLoad::Failed(err)) => ListState::Failed(err.clone()),
        Some(SchemaLoad::Ready(tables)) if tables.is_empty() => ListState::Empty,
        Some(SchemaLoad::Ready(_)) => ListState::Ready,
        Some(SchemaLoad::Loading) | None => ListState::Loading,
    };

    let raw = filter();
    let query = parse_query(&raw);
    let columns_mode = query.mode == FilterMode::Columns;
    // The box only helps once there is a list to narrow, and it would be a
    // confusing thing to offer next to a load error. Same for the
    // internal-objects note at the foot.
    let ready = list_state == ListState::Ready;
    let show_filter = ready;
    let show_internal_objects = state.show_internal_objects;
    let show_internal = show_internal_objects();

    // Memoized, not computed in the render body: matching runs over every
    // table (and in column mode every column) of the schema, so re-running it
    // on unrelated re-renders — selecting a table, staging an edit — would
    // re-pay the whole cost for nothing. Reads `schemas`, `filter` and
    // `show_internal_objects`, so it recomputes exactly when one of those
    // changes.
    let groups = use_memo(move || {
        let query = parse_query(&filter());
        let show_internal = show_internal_objects();
        let schemas = state.schemas.read();
        let Some(SchemaLoad::Ready(tables)) = schemas.get(&id) else {
            return FilteredGroups::new();
        };
        let hits = filter_tables(tables, &query, show_internal);
        group_by_schema(tables, &hits)
            .into_iter()
            .map(|(schema, group)| {
                let group = group
                    .into_iter()
                    .map(|hit| (tables[hit.index].clone(), hit.columns))
                    .collect();
                (schema, group)
            })
            .collect()
    });
    let groups = groups();
    // Its own memo: depends on the schema alone, so typing in the filter box
    // doesn't recount the whole list.
    let internal_count = use_memo(move || match state.schemas.read().get(&id) {
        Some(SchemaLoad::Ready(tables)) => count_internal_tables(tables),
        _ => 0,
    });
    let internal_count = internal_count();
    // A schema that contributed no hit contributes no header either, which is
    // what keeps the result reading as a narrowed tree rather than a flat list
    // of names.
    let show_headers =
        groups.len() > 1 || groups.first().is_some_and(|(schema, _)| schema.is_some());

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
                match list_state {
                    ListState::Loading => rsx! {
                        DelayedLoading { label: "Loading schema…" }
                    },
                    ListState::Failed(err) => rsx! {
                        div { class: "p-2",
                            Banner { kind: BannerKind::Error, message: err }
                        }
                    },
                    ListState::Empty => rsx! {
                        EmptyState {
                            icon: rsx! { Database { size: 40 } },
                            title: "No tables",
                            hint: "This database has no tables yet.",
                        }
                    },
                    // An empty needle matches everything in introspection
                    // order, so the unfiltered tree renders through this same
                    // path — there is no separate "no filter" branch to keep
                    // in sync.
                    ListState::Ready => rsx! {
                        if groups.is_empty() {
                            p { class: "px-3 py-4 text-center text-xs text-slate-500 dark:text-slate-400",
                                // With nothing typed the list can still come
                                // up empty, when every object the database has
                                // belongs to an extension and hiding is on
                                // (FRE-88) — "nothing matches ''" would be a
                                // nonsense way to say that.
                                if query.needle.is_empty() {
                                    "Everything in this database is an internal object."
                                } else {
                                    if columns_mode { "No columns match " } else { "No tables match " }
                                    span { class: "font-mono text-slate-700 dark:text-slate-300", "{query.needle}" }
                                }
                            }
                        }
                        for (schema , group) in groups {
                            if show_headers {
                                p { class: "px-2 pt-2 pb-0.5 text-xs font-semibold uppercase tracking-wide text-slate-400 dark:text-slate-600",
                                    {schema.clone().unwrap_or_else(|| "(no schema)".to_string())}
                                }
                            }
                            ul {
                                for (table , columns) in group {
                                    TableNode {
                                        key: "{table.schema:?}.{table.name}",
                                        id,
                                        table,
                                        columns,
                                    }
                                }
                            }
                        }
                    },
                }
            }
            // Only ever rendered by a database that actually has internal
            // objects, so the ordinary case carries no extra chrome. It says
            // the count rather than just offering a toggle: the point is that
            // the sidebar is not claiming to list everything (FRE-88).
            if ready && internal_count > 0 {
                div { class: "flex items-center justify-between gap-2 border-t border-slate-200 dark:border-slate-800 px-2 py-1.5 text-xs text-slate-500 dark:text-slate-400",
                    span {
                        title: "Objects the database created for itself — extension schemas like TimescaleDB's chunks, extension tables like PostGIS's spatial_ref_sys, and child partitions. They stay queryable in the SQL editor either way.",
                        if show_internal {
                            "{internal_count} internal objects shown"
                        } else {
                            "{internal_count} internal objects hidden"
                        }
                    }
                    button {
                        class: "shrink-0 rounded px-1.5 py-0.5 hover:bg-slate-200 dark:hover:bg-slate-800 hover:text-slate-900 dark:hover:text-slate-100",
                        title: "Show or hide the database's own internal objects",
                        onclick: move |_| state.set_show_internal_objects(!show_internal),
                        if show_internal { "Hide" } else { "Show" }
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
    // The engine's own word for what this is, when it has one — "hypertable"
    // rather than a table that inexplicably has chunks (FRE-88).
    let kind_label = table.read().kind_label.clone();
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
                    // Sits after the kind badge, since it refines it rather
                    // than replacing it: a continuous aggregate really is a
                    // view, and saying so stays true.
                    if let Some(label) = kind_label {
                        span { class: "shrink-0 rounded bg-teal-100 dark:bg-teal-900/50 px-1 text-xs text-teal-700 dark:text-teal-300",
                            "{label}"
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
