use dioxus::prelude::*;

use crate::db::{ConnectionId, Filter, FilterOp, PageRequest, QueryResult, SortDir, Value};

use super::state::{AppState, TableRef};

const PAGE_SIZE: u32 = 100;

/// Read-only paged grid for one table: sortable headers, per-column
/// contains/equals filter, page navigation, row-count indicator, refresh.
///
/// Callers key this component by table name, so all hook state here is
/// per-table and resets when another table is selected.
#[component]
pub fn DataGrid(id: ConnectionId, table: TableRef) -> Element {
    let state = use_context::<AppState>();
    let mut page = use_signal(|| 0u64);
    let mut sort = use_signal(|| Option::<(String, SortDir)>::None);
    // The filter inputs are staged locally and only hit the query when
    // applied, so typing doesn't fire a query per keystroke.
    let mut filter_column = use_signal(String::new);
    let mut filter_op = use_signal(|| FilterOp::Contains);
    let mut filter_text = use_signal(String::new);
    let mut applied_filter = use_signal(|| Option::<Filter>::None);
    let mut refresh_nonce = use_signal(|| 0u32);

    let table_for_resource = table.clone();
    let rows_resource = use_resource(move || {
        let table = table_for_resource.clone();
        // Read reactive deps before any await so the resource re-runs when
        // they change and no borrow spans the await.
        let request = PageRequest {
            schema: table.schema.clone(),
            table: table.name.clone(),
            limit: PAGE_SIZE,
            offset: page() * PAGE_SIZE as u64,
            sort: sort(),
            filter: applied_filter(),
        };
        let _ = refresh_nonce();
        let pool = state.registry.read().get(id).map(|c| c.pool.clone());
        async move {
            let Some(pool) = pool else {
                return Err(crate::db::DbError::Query("connection closed".into()));
            };
            let total = pool.count_rows(&request).await?;
            let result = pool.fetch_page(&request).await?;
            Ok::<(QueryResult, u64), crate::db::DbError>((result, total))
        }
    });

    // Column names for the filter dropdown and header fallback: prefer the
    // introspected schema so headers exist even for zero-row results.
    let schema_columns: Vec<String> = state
        .schemas
        .read()
        .get(&id)
        .and_then(|load| match load {
            super::state::SchemaLoad::Ready(tables) => tables
                .iter()
                .find(|t| t.name == table.name && t.schema == table.schema)
                .map(|t| t.columns.iter().map(|c| c.name.clone()).collect()),
            _ => None,
        })
        .unwrap_or_default();

    let current = rows_resource.read();
    let sort_value = sort();

    rsx! {
        div { class: "flex h-full min-h-0 flex-col",
            // Filter bar
            div { class: "flex items-center gap-2 border-b border-slate-800 px-3 py-2 text-sm",
                select {
                    class: "rounded border border-slate-700 bg-slate-950 px-2 py-1 text-xs text-slate-300",
                    onchange: move |evt| filter_column.set(evt.value()),
                    option { value: "", selected: filter_column.read().is_empty(), "column…" }
                    for column in schema_columns.clone() {
                        option {
                            value: "{column}",
                            selected: *filter_column.read() == column,
                            "{column}"
                        }
                    }
                }
                select {
                    class: "rounded border border-slate-700 bg-slate-950 px-2 py-1 text-xs text-slate-300",
                    onchange: move |evt| {
                        filter_op.set(if evt.value() == "equals" { FilterOp::Equals } else { FilterOp::Contains });
                    },
                    option { value: "contains", "contains" }
                    option { value: "equals", "equals" }
                }
                input {
                    class: "w-48 rounded border border-slate-700 bg-slate-950 px-2 py-1 font-mono text-xs text-slate-200 placeholder:text-slate-600",
                    placeholder: "filter value",
                    value: "{filter_text}",
                    oninput: move |evt| filter_text.set(evt.value()),
                    onkeydown: move |evt| {
                        if evt.key() == Key::Enter {
                            apply_filter(filter_column, filter_op, filter_text, applied_filter, page);
                        }
                    },
                }
                button {
                    class: "rounded bg-slate-700 px-3 py-1 text-xs text-slate-100 hover:bg-slate-600",
                    onclick: move |_| apply_filter(filter_column, filter_op, filter_text, applied_filter, page),
                    "Apply"
                }
                if applied_filter.read().is_some() {
                    button {
                        class: "rounded px-2 py-1 text-xs text-slate-400 hover:text-slate-100",
                        onclick: move |_| {
                            applied_filter.set(None);
                            filter_text.set(String::new());
                            page.set(0);
                        },
                        "Clear"
                    }
                }
                div { class: "flex-1" }
                button {
                    class: "rounded px-2 py-1 text-xs text-slate-400 hover:bg-slate-800 hover:text-slate-100",
                    title: "Re-run the current query",
                    onclick: move |_| { refresh_nonce += 1; },
                    "↻ Refresh"
                }
            }
            // Grid
            div { class: "min-h-0 flex-1 overflow-auto",
                match current.as_ref() {
                    None => rsx! {
                        p { class: "px-4 py-3 text-sm text-slate-500", "Loading…" }
                    },
                    Some(Err(err)) => rsx! {
                        p { class: "px-4 py-3 text-sm text-red-400", "{err}" }
                    },
                    Some(Ok((result, _total))) => {
                        let headers: Vec<String> = if result.columns.is_empty() {
                            schema_columns.clone()
                        } else {
                            result.columns.iter().map(|c| c.name.clone()).collect()
                        };
                        rsx! {
                            if result.rows.is_empty() {
                                p { class: "px-4 py-3 text-sm text-slate-500",
                                    if applied_filter.read().is_some() {
                                        "No rows match the filter."
                                    } else {
                                        "This table is empty."
                                    }
                                }
                            } else {
                                table { class: "w-full border-collapse text-left",
                                    thead { class: "sticky top-0 bg-slate-900",
                                        tr {
                                            for header in headers {
                                                GridHeader { name: header, sort: sort_value.clone(), on_sort: move |name: String| {
                                                    let next = next_sort(&sort.peek(), &name);
                                                    sort.set(next);
                                                    page.set(0);
                                                } }
                                            }
                                        }
                                    }
                                    tbody {
                                        for row in result.rows.iter() {
                                            tr { class: "border-t border-slate-800/60 hover:bg-slate-800/30",
                                                for value in row.iter() {
                                                    GridCell { value: value.clone() }
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
            // Footer: paging + counts
            div { class: "flex items-center gap-3 border-t border-slate-800 px-3 py-1.5 text-xs text-slate-400",
                match current.as_ref() {
                    Some(Ok((result, total))) => {
                        let first = if *total == 0 { 0 } else { page() * PAGE_SIZE as u64 + 1 };
                        let last = page() * PAGE_SIZE as u64 + result.rows.len() as u64;
                        rsx! {
                            span { "rows {first}–{last} of {total}" }
                            div { class: "flex-1" }
                            button {
                                class: "rounded px-2 py-0.5 hover:bg-slate-800 disabled:opacity-40",
                                disabled: page() == 0,
                                onclick: move |_| { let p = page(); page.set(p.saturating_sub(1)); },
                                "← Prev"
                            }
                            span { "page {page() + 1}" }
                            button {
                                class: "rounded px-2 py-0.5 hover:bg-slate-800 disabled:opacity-40",
                                disabled: last >= *total,
                                onclick: move |_| { let p = page(); page.set(p + 1); },
                                "Next →"
                            }
                        }
                    }
                    _ => rsx! {
                        span { "…" }
                    },
                }
            }
        }
    }
}

/// Applies the staged filter inputs and resets to the first page.
fn apply_filter(
    filter_column: Signal<String>,
    filter_op: Signal<FilterOp>,
    filter_text: Signal<String>,
    mut applied_filter: Signal<Option<Filter>>,
    mut page: Signal<u64>,
) {
    let column = filter_column.peek().clone();
    let value = filter_text.peek().clone();
    if column.is_empty() || value.is_empty() {
        applied_filter.set(None);
    } else {
        applied_filter.set(Some(Filter {
            column,
            op: *filter_op.peek(),
            value,
        }));
    }
    page.set(0);
}

/// Sort cycles none → asc → desc → none per column.
fn next_sort(current: &Option<(String, SortDir)>, column: &str) -> Option<(String, SortDir)> {
    match current {
        Some((c, SortDir::Asc)) if c == column => Some((column.to_string(), SortDir::Desc)),
        Some((c, SortDir::Desc)) if c == column => None,
        _ => Some((column.to_string(), SortDir::Asc)),
    }
}

#[component]
fn GridHeader(
    name: String,
    sort: Option<(String, SortDir)>,
    on_sort: EventHandler<String>,
) -> Element {
    let marker = match &sort {
        Some((c, SortDir::Asc)) if *c == name => " ▲",
        Some((c, SortDir::Desc)) if *c == name => " ▼",
        _ => "",
    };
    let clicked_name = name.clone();
    rsx! {
        th { class: "border-b border-slate-700 px-3 py-1.5",
            button {
                class: "font-mono text-xs font-semibold text-slate-300 hover:text-white",
                onclick: move |_| on_sort.call(clicked_name.clone()),
                "{name}{marker}"
            }
        }
    }
}

/// One cell: monospace, truncated long text (full value in the tooltip),
/// NULL and blobs rendered distinctly.
#[component]
fn GridCell(value: Value) -> Element {
    let display = value.display();
    let class = match &value {
        Value::Null => "px-3 py-1 font-mono text-xs italic text-slate-600",
        Value::Blob(_) => "px-3 py-1 font-mono text-xs text-violet-400",
        _ => "px-3 py-1 font-mono text-xs text-slate-200",
    };
    rsx! {
        td { class,
            div { class: "max-w-md truncate", title: "{display}", "{display}" }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sort_cycles_per_column() {
        let none = None;
        let asc = next_sort(&none, "a");
        assert_eq!(asc, Some(("a".to_string(), SortDir::Asc)));
        let desc = next_sort(&asc, "a");
        assert_eq!(desc, Some(("a".to_string(), SortDir::Desc)));
        assert_eq!(next_sort(&desc, "a"), None);
    }

    #[test]
    fn sorting_another_column_starts_at_asc() {
        let current = Some(("a".to_string(), SortDir::Desc));
        assert_eq!(
            next_sort(&current, "b"),
            Some(("b".to_string(), SortDir::Asc))
        );
    }
}
