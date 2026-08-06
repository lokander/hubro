use dioxus::prelude::*;
use dioxus_icons::lucide::{Database, RefreshCw};

use crate::db::{ConnectionId, TableKind, TableMeta};

use super::notice::{Banner, BannerKind, DelayedLoading, EmptyState};
use super::state::{AppState, SchemaLoad, TableRef};

/// Sidebar for one connection: the introspected schema as an expandable
/// tree. Tables and views expand to columns (with PK/NOT NULL markers) and
/// indexes; clicking a name selects it for the data grid.
#[component]
pub fn SchemaSidebar(id: ConnectionId) -> Element {
    let state = use_context::<AppState>();
    let schema = state
        .schemas
        .read()
        .get(&id)
        .cloned()
        .unwrap_or(SchemaLoad::Loading);

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
                        // Group by schema (Postgres); SQLite tables have no
                        // schema and render as a flat list.
                        let mut groups: Vec<(Option<String>, Vec<TableMeta>)> = Vec::new();
                        for table in tables {
                            match groups.last_mut() {
                                Some((schema, group)) if *schema == table.schema => {
                                    group.push(table)
                                }
                                _ => groups.push((table.schema.clone(), vec![table])),
                            }
                        }
                        let show_headers = groups.len() > 1
                            || groups.first().is_some_and(|(s, _)| s.is_some());
                        rsx! {
                            for (schema, group) in groups {
                                if show_headers {
                                    p { class: "px-2 pt-2 pb-0.5 text-xs font-semibold uppercase tracking-wide text-slate-400 dark:text-slate-600",
                                        {schema.clone().unwrap_or_else(|| "(no schema)".to_string())}
                                    }
                                }
                                ul {
                                    for table in group {
                                        TableNode { key: "{table.schema:?}.{table.name}", id, table }
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
/// in the Schema pane now (FRE-69), so there is nothing to expand.
#[component]
fn TableNode(id: ConnectionId, table: ReadSignal<TableMeta>) -> Element {
    let state = use_context::<AppState>();
    let name = table.read().name.clone();
    let kind = table.read().kind;
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
                    // Enter/Space natively via this onclick.
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
        }
    }
}
