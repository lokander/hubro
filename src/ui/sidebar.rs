use dioxus::prelude::*;

use crate::db::{ConnectionId, TableKind, TableMeta};

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
            div { class: "flex items-center justify-between border-b border-slate-800 px-4 py-2",
                span { class: "text-xs font-semibold uppercase tracking-wide text-slate-500",
                    "Schema"
                }
                button {
                    class: "rounded px-2 py-0.5 text-xs text-slate-400 hover:bg-slate-800 hover:text-slate-100",
                    title: "Reload schema",
                    onclick: move |_| state.load_schema(id),
                    "↻ Reload"
                }
            }
            div { class: "min-h-0 flex-1 overflow-y-auto py-1",
                match schema {
                    SchemaLoad::Loading => rsx! {
                        p { class: "px-4 py-2 text-sm text-slate-500", "Loading schema…" }
                    },
                    SchemaLoad::Failed(err) => rsx! {
                        p { class: "px-4 py-2 text-sm text-red-400", "{err}" }
                    },
                    SchemaLoad::Ready(tables) if tables.is_empty() => rsx! {
                        p { class: "px-4 py-2 text-sm text-slate-500", "This database has no tables." }
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
                                    p { class: "px-2 pt-2 pb-0.5 text-xs font-semibold uppercase tracking-wide text-slate-600",
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

/// One table/view in the tree: a row with expand toggle + name, and when
/// expanded, its columns and indexes.
#[component]
fn TableNode(id: ConnectionId, table: ReadSignal<TableMeta>) -> Element {
    let state = use_context::<AppState>();
    let name = table.read().name.clone();
    let kind = table.read().kind;
    let table_ref = TableRef {
        schema: table.read().schema.clone(),
        name: name.clone(),
    };
    let key = table_ref.key();
    let (expanded, selected) = {
        let tab_ui = state.tab_ui.read();
        match tab_ui.get(&id) {
            Some(ui) => (
                ui.expanded.contains(&key),
                ui.selected_table.as_ref() == Some(&table_ref),
            ),
            None => (false, false),
        }
    };

    let toggle_key = key.clone();
    let select_ref = table_ref.clone();
    rsx! {
        li {
            div {
                class: if selected {
                    "flex items-center gap-1 bg-sky-900/40 px-2 py-1 text-sm text-sky-200"
                } else {
                    "flex items-center gap-1 px-2 py-1 text-sm text-slate-300 hover:bg-slate-800/60"
                },
                button {
                    class: "w-4 shrink-0 text-xs text-slate-500 hover:text-slate-200",
                    aria_label: if expanded { "Collapse" } else { "Expand" },
                    onclick: move |_| state.toggle_expanded(id, &toggle_key),
                    if expanded { "▾" } else { "▸" }
                }
                button {
                    class: "flex min-w-0 flex-1 items-center gap-2 text-left",
                    onclick: move |_| state.select_table(id, &select_ref),
                    span { class: "truncate font-mono", "{name}" }
                    if kind == TableKind::View {
                        span { class: "rounded bg-violet-900/50 px-1 text-xs text-violet-300",
                            "view"
                        }
                    }
                }
            }
            if expanded {
                TableDetails { table }
            }
        }
    }
}

#[component]
fn TableDetails(table: ReadSignal<TableMeta>) -> Element {
    let table = table.read();
    rsx! {
        ul { class: "border-l border-slate-800 pb-1 pl-3 ml-4",
            for column in table.columns.clone() {
                li { class: "flex items-baseline gap-2 px-2 py-0.5 text-xs",
                    span { class: "font-mono text-slate-300", "{column.name}" }
                    span { class: "font-mono text-slate-500",
                        {if column.type_name.is_empty() { "any".to_string() } else { column.type_name.to_lowercase() }}
                    }
                    if column.primary_key_position.is_some() {
                        span { class: "rounded bg-amber-900/50 px-1 text-amber-300", "PK" }
                    }
                    if !column.nullable {
                        span { class: "text-slate-600", "not null" }
                    }
                }
            }
            if !table.indexes.is_empty() {
                li { class: "mt-1 px-2 text-xs uppercase tracking-wide text-slate-600", "Indexes" }
                for index in table.indexes.clone() {
                    li { class: "flex items-baseline gap-2 px-2 py-0.5 text-xs",
                        span { class: "truncate font-mono text-slate-400", "{index.name}" }
                        span { class: "font-mono text-slate-600",
                            "({index.columns.join(\", \")})"
                        }
                        if index.unique {
                            span { class: "rounded bg-emerald-900/50 px-1 text-emerald-300",
                                "unique"
                            }
                        }
                        if index.partial {
                            span { class: "rounded bg-slate-800 px-1 text-slate-400", "partial" }
                        }
                    }
                }
            }
        }
    }
}
