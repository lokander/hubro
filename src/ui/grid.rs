use dioxus::prelude::*;

use crate::db::{
    detect_row_identity, ConnectionId, Dialect, Filter, FilterOp, PageRequest, QueryResult,
    RowIdentity, RowLocator, SortDir, TableKind, TableMeta, Value,
};

use super::stage::TableStage;
use super::state::{AppState, SchemaLoad, TableRef};

const PAGE_SIZE: u32 = 100;

/// Paged grid for one table: sortable headers, per-column contains/equals
/// filter, page navigation, row-count indicator, refresh — plus staged-edit
/// rendering (FRE-14): dirty cells and deletes are tinted, pending inserts
/// show as phantom rows, and a Save/Discard bar appears while the table's
/// stage is non-empty.
///
/// Callers key this component by table name, so all hook state here is
/// per-table and resets when another table is selected. (The refresh nonce
/// is NOT local: it lives in [`AppState::grid_refresh`] so a successful save
/// can force a refetch from outside the component.)
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

    let table_for_resource = table.clone();
    let rows_resource = use_resource(move || {
        let table = table_for_resource.clone();
        // Read reactive deps before any await so the resource re-runs when
        // they change and no borrow spans the await.
        //
        // Rowid-identity tables (SQLite, keyless) need the rowid in every
        // fetched row to build row locators, but `SELECT *` doesn't include
        // it — ask the page reader for it explicitly. The fetch's extra
        // column is returned alongside the result so rendering hides
        // exactly what this fetch prepended (never a stale render-time
        // guess).
        let extra_key_column = {
            let dialect = state.registry.read().get(id).map(|c| c.pool.dialect());
            let schemas = state.schemas.read();
            let meta = find_table(schemas.get(&id), &table);
            match (meta, dialect) {
                (Some(meta), Some(dialect)) => match detect_row_identity(meta, dialect) {
                    Some(RowIdentity::Rowid { column }) => Some(column),
                    _ => None,
                },
                _ => None,
            }
        };
        let request = PageRequest {
            schema: table.schema.clone(),
            table: table.name.clone(),
            limit: PAGE_SIZE,
            offset: page() * PAGE_SIZE as u64,
            sort: sort(),
            filter: applied_filter(),
            extra_key_column,
        };
        let _ = state.grid_refresh.read().get(&(id, table.key())).copied();
        let pool = state.registry.read().get(id).map(|c| c.pool.clone());
        async move {
            let Some(pool) = pool else {
                return Err(crate::db::DbError::Query("connection closed".into()));
            };
            let total = pool.count_rows(&request).await?;
            let result = pool.fetch_page(&request).await?;
            Ok::<(QueryResult, u64, Option<String>), crate::db::DbError>((
                result,
                total,
                request.extra_key_column,
            ))
        }
    });

    // Introspected metadata for this table: column names feed the filter
    // dropdown and header fallback (so headers exist even for zero-row
    // results), and row-identity detection decides the read-only notice and
    // how staged rows are addressed.
    let table_meta: Option<TableMeta> = find_table(state.schemas.read().get(&id), &table).cloned();
    let schema_columns: Vec<String> = table_meta
        .as_ref()
        .map(|t| t.columns.iter().map(|c| c.name.clone()).collect())
        .unwrap_or_default();

    let dialect: Option<Dialect> = state.registry.read().get(id).map(|c| c.pool.dialect());
    let identity: Option<RowIdentity> = match (&table_meta, dialect) {
        (Some(meta), Some(dialect)) => detect_row_identity(meta, dialect),
        _ => None,
    };
    // Editing (FRE-24+) will consult the same detection; until then the
    // notice explains up front why these rows will stay read-only.
    let read_only_notice: Option<&'static str> = match (&table_meta, identity.is_none()) {
        (Some(meta), true) => {
            if meta.kind == TableKind::View {
                Some("Views are read-only.")
            } else {
                Some(
                    "This table has no primary key or usable unique index — \
                     editing will be disabled.",
                )
            }
        }
        _ => None,
    };

    // Staged (unsaved) changes of this table, if any.
    let stage: Option<TableStage> = state.table_stage(id, &table);
    let pending_count = stage.as_ref().map(TableStage::pending_count).unwrap_or(0);
    let saving = stage.as_ref().is_some_and(|s| s.saving);
    let save_error = stage.as_ref().and_then(|s| s.last_error.clone());
    // The two-step navigation guard parks blocked navigations here; the Save
    // bar explains how to proceed (see AppState::nav_guard for the UX).
    let nav_blocked = state
        .nav_guard
        .read()
        .as_ref()
        .is_some_and(|nav| nav.id == id);

    let current = rows_resource.read();
    let sort_value = sort();
    let refresh_table = table.clone();
    let save_table = table.clone();
    let discard_table = table.clone();

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
                    onclick: move |_| state.bump_grid_refresh(id, &refresh_table.key()),
                    "↻ Refresh"
                }
            }
            // Save/Discard bar: appears while this table has staged changes.
            if pending_count > 0 {
                div { class: "flex items-center gap-3 border-b border-amber-700/50 bg-amber-950/40 px-3 py-1.5 text-xs",
                    span { class: "font-semibold text-amber-300",
                        if pending_count == 1 { "1 pending change" } else { "{pending_count} pending changes" }
                    }
                    if nav_blocked {
                        span { class: "text-amber-200",
                            "Unsaved changes — Save or Discard first (repeat the action to discard & leave)."
                        }
                    }
                    if let Some(error) = save_error {
                        span { class: "min-w-0 flex-1 truncate text-red-400", title: "{error}", "{error}" }
                    } else {
                        div { class: "flex-1" }
                    }
                    button {
                        class: "rounded bg-emerald-700 px-3 py-1 font-semibold text-white hover:bg-emerald-600 disabled:opacity-40",
                        disabled: saving,
                        onclick: move |_| state.save_staged(id, &save_table),
                        if saving { "Saving…" } else { "Save" }
                    }
                    button {
                        class: "rounded border border-slate-600 px-3 py-1 text-slate-300 hover:bg-slate-800",
                        disabled: saving,
                        onclick: move |_| state.discard_staged(id, &discard_table),
                        "Discard"
                    }
                }
            }
            // Read-only notice (views / no usable row key)
            if let Some(notice) = read_only_notice {
                div { class: "border-b border-slate-800 bg-slate-800/60 px-3 py-1.5 text-xs text-slate-300",
                    "{notice}"
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
                    Some(Ok((result, _total, extra_key))) => {
                        // The fetch prepended the row-identity key column
                        // (rowid) when one was requested; keep it for
                        // locators, hide it from display.
                        let hidden = usize::from(extra_key.is_some());
                        let headers: Vec<String> = if result.columns.is_empty() {
                            schema_columns.clone()
                        } else {
                            result.columns.iter().skip(hidden).map(|c| c.name.clone()).collect()
                        };
                        let rows = view_rows(result, hidden, identity.as_ref(), stage.as_ref());
                        let inserts = insert_rows(&headers, stage.as_ref());
                        rsx! {
                            if rows.is_empty() && inserts.is_empty() {
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
                                        for row in rows {
                                            tr {
                                                class: if row.deleted {
                                                    // Pending delete: red tint + strike-through.
                                                    "border-t border-slate-800/60 bg-red-950/40 line-through decoration-red-400/60"
                                                } else {
                                                    "border-t border-slate-800/60 hover:bg-slate-800/30"
                                                },
                                                for cell in row.cells {
                                                    GridCell { value: cell.value, dirty: cell.dirty }
                                                }
                                            }
                                        }
                                        // Pending inserts: phantom rows, green tint.
                                        for insert_row in inserts {
                                            tr { class: "border-t border-slate-800/60 bg-emerald-950/40",
                                                for cell in insert_row {
                                                    GridCell { value: cell.value, dirty: cell.dirty }
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
                    Some(Ok((result, total, _))) => {
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

/// Finds one table's metadata in a loaded schema.
fn find_table<'a>(load: Option<&'a SchemaLoad>, table: &TableRef) -> Option<&'a TableMeta> {
    match load? {
        SchemaLoad::Ready(tables) => tables
            .iter()
            .find(|t| t.name == table.name && t.schema == table.schema),
        _ => None,
    }
}

/// One cell prepared for rendering.
struct CellView {
    value: Value,
    dirty: bool,
}

/// One fetched row prepared for rendering, staged state applied.
struct RowView {
    deleted: bool,
    cells: Vec<CellView>,
}

/// Applies the stage to the fetched page: computes each row's locator from
/// the identity's key columns (matched by name against the result), then
/// substitutes staged cell values (dirty) and flags pending deletes. Rows
/// whose key columns are missing from the result (transient schema/result
/// mismatch) render clean — they can't be addressed, so they can't be dirty.
fn view_rows(
    result: &QueryResult,
    hidden: usize,
    identity: Option<&RowIdentity>,
    stage: Option<&TableStage>,
) -> Vec<RowView> {
    // Indices of the identity's key columns within the result (`None` when
    // any key column is missing from the fetched page).
    let key_indices: Option<Vec<usize>> = identity.and_then(|identity| {
        identity
            .key_columns()
            .iter()
            .map(|key| result.columns.iter().position(|c| c.name == *key))
            .collect()
    });
    let mut rows = Vec::with_capacity(result.rows.len());
    for row in &result.rows {
        let row_key: Option<String> = match (&key_indices, stage) {
            (Some(indices), Some(_)) => Some(
                RowLocator {
                    identity_values: indices.iter().map(|&i| row[i].clone()).collect(),
                }
                .key(),
            ),
            _ => None,
        };
        let deleted =
            matches!((&row_key, stage), (Some(key), Some(stage)) if stage.is_deleted(key));
        let cells = row
            .iter()
            .enumerate()
            .skip(hidden)
            .map(|(index, value)| {
                let staged = match (&row_key, stage) {
                    (Some(key), Some(stage)) => {
                        stage.edited_value(key, &result.columns[index].name)
                    }
                    _ => None,
                };
                CellView {
                    dirty: staged.is_some(),
                    value: staged.unwrap_or(value).clone(),
                }
            })
            .collect();
        rows.push(RowView { deleted, cells });
    }
    rows
}

/// Pending inserts as phantom rows aligned to the visible headers; columns
/// the insert doesn't set render as NULL-styled placeholders.
fn insert_rows(headers: &[String], stage: Option<&TableStage>) -> Vec<Vec<CellView>> {
    let Some(stage) = stage else {
        return Vec::new();
    };
    stage
        .inserts()
        .iter()
        .map(|insert| {
            headers
                .iter()
                .map(|header| {
                    let value = insert
                        .columns
                        .iter()
                        .position(|c| c == header)
                        .map(|i| insert.values[i].clone())
                        .unwrap_or(Value::Null);
                    CellView { value, dirty: true }
                })
                .collect()
        })
        .collect()
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
/// NULL and blobs rendered distinctly. A dirty cell (pending staged edit)
/// shows the *staged* value on an amber tint.
#[component]
fn GridCell(value: Value, dirty: bool) -> Element {
    let display = value.display();
    let text = match &value {
        Value::Null => "font-mono text-xs italic text-slate-600",
        Value::Blob(_) => "font-mono text-xs text-violet-400",
        _ => "font-mono text-xs text-slate-200",
    };
    let class = if dirty {
        format!("px-3 py-1 {text} bg-amber-900/40")
    } else {
        format!("px-3 py-1 {text}")
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

    fn two_column_result() -> QueryResult {
        QueryResult {
            columns: vec![
                crate::db::ColumnInfo { name: "id".into() },
                crate::db::ColumnInfo {
                    name: "title".into(),
                },
            ],
            rows: vec![
                vec![Value::Integer(1), Value::Text("one".into())],
                vec![Value::Integer(2), Value::Text("two".into())],
            ],
        }
    }

    fn pk_identity() -> RowIdentity {
        RowIdentity::PrimaryKey {
            columns: vec!["id".into()],
        }
    }

    #[test]
    fn staged_edits_and_deletes_mark_the_right_rows() {
        let mut stage = TableStage::default();
        stage.set_cell_edit(
            RowLocator {
                identity_values: vec![Value::Integer(1)],
            },
            "title",
            Value::Text("edited".into()),
        );
        stage.mark_delete(RowLocator {
            identity_values: vec![Value::Integer(2)],
        });
        let result = two_column_result();
        let rows = view_rows(&result, 0, Some(&pk_identity()), Some(&stage));
        assert_eq!(rows.len(), 2);
        // Row 1: title cell dirty, showing the staged value; id cell clean.
        assert!(!rows[0].deleted);
        assert!(!rows[0].cells[0].dirty);
        assert!(rows[0].cells[1].dirty);
        assert_eq!(rows[0].cells[1].value, Value::Text("edited".into()));
        // Row 2: pending delete.
        assert!(rows[1].deleted);
        assert!(!rows[1].cells[1].dirty);
        assert_eq!(rows[1].cells[1].value, Value::Text("two".into()));
    }

    #[test]
    fn hidden_key_column_feeds_locators_but_not_cells() {
        // A rowid fetch: first column is the hidden rowid.
        let result = QueryResult {
            columns: vec![
                crate::db::ColumnInfo {
                    name: "rowid".into(),
                },
                crate::db::ColumnInfo {
                    name: "body".into(),
                },
            ],
            rows: vec![vec![Value::Integer(7), Value::Text("note".into())]],
        };
        let identity = RowIdentity::Rowid {
            column: "rowid".into(),
        };
        let mut stage = TableStage::default();
        stage.set_cell_edit(
            RowLocator {
                identity_values: vec![Value::Integer(7)],
            },
            "body",
            Value::Text("edited".into()),
        );
        let rows = view_rows(&result, 1, Some(&identity), Some(&stage));
        // The rowid column is hidden; the one visible cell is the dirty body.
        assert_eq!(rows[0].cells.len(), 1);
        assert!(rows[0].cells[0].dirty);
        assert_eq!(rows[0].cells[0].value, Value::Text("edited".into()));
    }

    #[test]
    fn rows_without_identity_render_clean() {
        let mut stage = TableStage::default();
        stage.set_cell_edit(
            RowLocator {
                identity_values: vec![Value::Integer(1)],
            },
            "title",
            Value::Text("edited".into()),
        );
        let result = two_column_result();
        let rows = view_rows(&result, 0, None, Some(&stage));
        assert!(rows.iter().all(|r| !r.deleted));
        assert!(rows.iter().flat_map(|r| r.cells.iter()).all(|c| !c.dirty));
    }

    #[test]
    fn insert_rows_align_to_headers_and_default_missing_columns_to_null() {
        let mut stage = TableStage::default();
        stage.add_insert(vec!["title".into()], vec![Value::Text("new".into())]);
        let headers = vec!["id".to_string(), "title".to_string()];
        let rows = insert_rows(&headers, Some(&stage));
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0][0].value, Value::Null);
        assert_eq!(rows[0][1].value, Value::Text("new".into()));
        assert!(rows[0].iter().all(|c| c.dirty));
        assert!(insert_rows(&headers, None).is_empty());
    }
}
