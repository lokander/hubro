use std::collections::HashMap;

use dioxus::prelude::*;

use crate::db::{
    detect_row_identity, ConnectionId, Dialect, Filter, FilterOp, PageRequest, QueryResult,
    RowIdentity, RowLocator, SortDir, TableKind, TableMeta, Value,
};

use super::editing::{editor_kind, CellEditor, EditNav, EditorKind};
use super::stage::TableStage;
use super::state::{AppState, SchemaLoad, TableRef};

const PAGE_SIZE: u32 = 100;

/// The cell whose in-place editor is open, addressed by row key
/// ([`RowLocator::key`]) + column name. At most one editor is open per
/// grid.
#[derive(Debug, Clone, PartialEq)]
struct ActiveEdit {
    row_key: String,
    column: String,
}

/// Paged grid for one table: sortable headers, per-column contains/equals
/// filter, page navigation, row-count indicator, refresh — plus staged-edit
/// rendering (FRE-14): dirty cells and deletes are tinted, pending inserts
/// show as phantom rows, and a Save/Discard bar appears while the table's
/// stage is non-empty. Cells of editable rows open a type-aware in-place
/// editor (FRE-24, see [`CellEditor`]) on double-click or Enter; commits
/// only ever stage (never write through).
///
/// Callers key this component by table name, so all hook state here is
/// per-table and resets when another table is selected. (The refresh nonce
/// is NOT local: it lives in [`AppState::grid_refresh`] so a successful save
/// can force a refetch from outside the component.)
#[component]
pub fn DataGrid(id: ConnectionId, table: TableRef) -> Element {
    let state = use_context::<AppState>();
    let mut page = use_signal(|| 0u64);
    // Which cell's editor is open. Survives refetches by row key: if the
    // page shifts under the editor the addressed row simply isn't rendered
    // and the editor disappears (no stale locator can be committed).
    let editing = use_signal(|| Option::<ActiveEdit>::None);
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
    // Per-column editor kind + nullability, from introspection. Cells whose
    // column is missing here (transient schema/result mismatch) fall back
    // to a plain text editor on a nullable column.
    let column_kinds: HashMap<String, (EditorKind, bool)> = table_meta
        .as_ref()
        .map(|meta| {
            meta.columns
                .iter()
                .map(|c| (c.name.clone(), (editor_kind(&c.type_name), c.nullable)))
                .collect()
        })
        .unwrap_or_default();
    // Editing consults the same identity detection: no identity → no
    // editors; the notice explains up front why the rows are read-only.
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
    let row_table = table.clone();

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
                                        for (index, row) in rows.into_iter().enumerate() {
                                            GridRow {
                                                key: "{row_render_key(&row, index)}",
                                                id,
                                                table: row_table.clone(),
                                                row,
                                                column_kinds: column_kinds.clone(),
                                                dialect: dialect.unwrap_or(Dialect::Sqlite),
                                                editing,
                                            }
                                        }
                                        // Pending inserts: phantom rows, green tint
                                        // (not editable in place — FRE-25 owns insert UX).
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
#[derive(Debug, Clone, PartialEq)]
struct CellView {
    column: String,
    value: Value,
    dirty: bool,
    /// Row-level editability: the row has a locator, is not pending
    /// deletion, and this cell's fetched value is not a blob. Column-type
    /// restrictions (blob-typed columns) apply on top at render time.
    editable: bool,
}

/// One fetched row prepared for rendering, staged state applied.
#[derive(Debug, Clone, PartialEq)]
struct RowView {
    /// [`RowLocator::key`] of `locator`, when the row is addressable.
    key: Option<String>,
    /// How staged edits address this row (`None` when the table has no
    /// identity or a key column is missing from the fetched page).
    locator: Option<RowLocator>,
    deleted: bool,
    cells: Vec<CellView>,
}

/// Applies the stage to the fetched page: computes each row's locator from
/// the identity's key columns (matched by name against the result), then
/// substitutes staged cell values (dirty) and flags pending deletes. Rows
/// whose key columns are missing from the result (transient schema/result
/// mismatch) render clean and read-only — they can't be addressed.
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
        let locator: Option<RowLocator> = key_indices.as_ref().map(|indices| RowLocator {
            identity_values: indices.iter().map(|&i| row[i].clone()).collect(),
        });
        let row_key: Option<String> = locator.as_ref().map(RowLocator::key);
        let deleted =
            matches!((&row_key, stage), (Some(key), Some(stage)) if stage.is_deleted(key));
        let cells = row
            .iter()
            .enumerate()
            .skip(hidden)
            .map(|(index, value)| {
                let column = result.columns[index].name.clone();
                let staged = match (&row_key, stage) {
                    (Some(key), Some(stage)) => stage.edited_value(key, &column),
                    _ => None,
                };
                CellView {
                    dirty: staged.is_some(),
                    editable: locator.is_some() && !deleted && !matches!(value, Value::Blob(_)),
                    value: staged.unwrap_or(value).clone(),
                    column,
                }
            })
            .collect();
        rows.push(RowView {
            key: row_key,
            locator,
            deleted,
            cells,
        });
    }
    rows
}

/// Stable list key for one row: the row key when the row is addressable,
/// else its page position.
fn row_render_key(row: &RowView, index: usize) -> String {
    match &row.key {
        Some(key) => format!("r{key}"),
        None => format!("i{index}"),
    }
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
                    CellView {
                        column: header.clone(),
                        value,
                        dirty: true,
                        editable: false,
                    }
                })
                .collect()
        })
        .collect()
}

/// Whether a cell may open an editor: the row allows it (locator present,
/// not deleted, value not a blob) and the column's type is editable
/// (blob-typed columns are read-only for now).
fn cell_editable(cell: &CellView, column_kinds: &HashMap<String, (EditorKind, bool)>) -> bool {
    cell.editable && cell_kind(cell, column_kinds).0 != EditorKind::Blob
}

/// Editor kind + nullability for one cell; columns missing from the
/// introspected metadata edit as nullable text.
fn cell_kind(
    cell: &CellView,
    column_kinds: &HashMap<String, (EditorKind, bool)>,
) -> (EditorKind, bool) {
    column_kinds
        .get(&cell.column)
        .copied()
        .unwrap_or((EditorKind::Text, true))
}

/// The editable column after (`+1`) or before (`-1`) `current` in a row's
/// Tab order; `None` at the row's edge (the editor then just closes).
fn step_column(columns: &[String], current: &str, delta: i32) -> Option<String> {
    let position = columns.iter().position(|c| c == current)?;
    let next = position as i64 + i64::from(delta);
    if next < 0 {
        return None;
    }
    columns.get(next as usize).cloned()
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

/// One fetched row: staged tint/strike-through, and one [`GridCellSlot`]
/// per cell (which renders either the display cell or, for the active
/// cell, the in-place editor).
#[component]
fn GridRow(
    id: ConnectionId,
    table: TableRef,
    row: RowView,
    column_kinds: HashMap<String, (EditorKind, bool)>,
    dialect: Dialect,
    editing: Signal<Option<ActiveEdit>>,
) -> Element {
    // This row's Tab order: its editable columns, left to right.
    let editable_columns: Vec<String> = row
        .cells
        .iter()
        .filter(|cell| cell_editable(cell, &column_kinds))
        .map(|cell| cell.column.clone())
        .collect();
    rsx! {
        tr {
            class: if row.deleted {
                // Pending delete: red tint + strike-through.
                "border-t border-slate-800/60 bg-red-950/40 line-through decoration-red-400/60"
            } else {
                "border-t border-slate-800/60 hover:bg-slate-800/30"
            },
            for cell in row.cells.clone() {
                GridCellSlot {
                    key: "{cell.column}",
                    id,
                    table: table.clone(),
                    row_key: row.key.clone(),
                    locator: row.locator.clone(),
                    kind: cell_kind(&cell, &column_kinds).0,
                    nullable: cell_kind(&cell, &column_kinds).1,
                    editable: cell_editable(&cell, &column_kinds),
                    cell: cell.clone(),
                    dialect,
                    editable_columns: editable_columns.clone(),
                    editing,
                }
            }
        }
    }
}

/// One cell of an editable-capable row: the display cell normally, or the
/// [`CellEditor`] while this cell is the grid's active edit. Commits stage
/// through [`AppState::stage_cell_edit`] — never the database — and Tab
/// commits walk `editable_columns`.
#[component]
fn GridCellSlot(
    id: ConnectionId,
    table: TableRef,
    row_key: Option<String>,
    locator: Option<RowLocator>,
    cell: CellView,
    kind: EditorKind,
    nullable: bool,
    editable: bool,
    dialect: Dialect,
    editable_columns: Vec<String>,
    mut editing: Signal<Option<ActiveEdit>>,
) -> Element {
    let state = use_context::<AppState>();
    let is_active = editable
        && editing.read().as_ref().is_some_and(|active| {
            row_key.as_deref() == Some(active.row_key.as_str()) && active.column == cell.column
        });

    if is_active {
        // `editable` guarantees the locator and row key exist.
        let locator = locator.clone().expect("editable cell has a locator");
        let row_key = row_key.clone().expect("editable cell has a row key");
        let column = cell.column.clone();
        let commit_table = table.clone();
        rsx! {
            CellEditor {
                kind,
                dialect,
                nullable,
                initial: cell.value.clone(),
                on_commit: move |(value, nav): (Option<Value>, EditNav)| {
                    if let Some(value) = value {
                        state.stage_cell_edit(id, &commit_table, locator.clone(), &column, value);
                    }
                    let next = match nav {
                        EditNav::Stay => None,
                        EditNav::Next => step_column(&editable_columns, &column, 1),
                        EditNav::Prev => step_column(&editable_columns, &column, -1),
                    };
                    editing.set(next.map(|column| ActiveEdit {
                        row_key: row_key.clone(),
                        column,
                    }));
                },
                on_cancel: move |_| editing.set(None),
            }
        }
    } else {
        let activate_key = row_key.clone();
        let column = cell.column.clone();
        let mut activate = move || {
            if let Some(row_key) = &activate_key {
                editing.set(Some(ActiveEdit {
                    row_key: row_key.clone(),
                    column: column.clone(),
                }));
            }
        };
        let mut open_on_enter = activate.clone();
        // Blob cells (by value or column type) explain why they're locked;
        // other read-only cells (views, keyless tables) are covered by the
        // grid-level notice.
        let blob_locked = kind == EditorKind::Blob || matches!(cell.value, Value::Blob(_));
        let display = cell.value.display();
        let tooltip = if blob_locked {
            "blobs are read-only".to_string()
        } else {
            display.clone()
        };
        let text = match &cell.value {
            Value::Null => "font-mono text-xs italic text-slate-600",
            Value::Blob(_) => "font-mono text-xs text-violet-400",
            _ => "font-mono text-xs text-slate-200",
        };
        let class = if cell.dirty {
            format!("px-3 py-1 {text} bg-amber-900/40")
        } else {
            format!("px-3 py-1 {text}")
        };
        rsx! {
            td {
                class,
                // Editable cells are focusable so Enter can open the editor
                // without a mouse.
                tabindex: if editable { "0" },
                ondoubleclick: move |_| {
                    if editable {
                        activate();
                    }
                },
                onkeydown: move |evt| {
                    if editable && evt.key() == Key::Enter {
                        open_on_enter();
                    }
                },
                div { class: "max-w-md truncate", title: "{tooltip}", "{display}" }
            }
        }
    }
}

/// One display-only cell (used for pending-insert phantom rows): monospace,
/// truncated long text (full value in the tooltip), NULL and blobs rendered
/// distinctly. A dirty cell shows the *staged* value on an amber tint.
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
    fn editability_needs_a_locator_and_excludes_deletes_and_blobs() {
        let kinds: HashMap<String, (EditorKind, bool)> = [
            (
                "id".to_string(),
                (EditorKind::Numeric { decimal: false }, false),
            ),
            ("title".to_string(), (EditorKind::Text, true)),
            ("cover".to_string(), (EditorKind::Blob, true)),
        ]
        .into_iter()
        .collect();

        // With an identity, plain cells are editable…
        let result = two_column_result();
        let rows = view_rows(&result, 0, Some(&pk_identity()), None);
        assert!(rows[0].locator.is_some());
        assert!(cell_editable(&rows[0].cells[1], &kinds));
        // …a column missing from the metadata falls back to editable text…
        let (kind, nullable) = cell_kind(&rows[0].cells[1], &HashMap::new());
        assert_eq!(kind, EditorKind::Text);
        assert!(nullable);
        // …but without an identity nothing is.
        let rows = view_rows(&result, 0, None, None);
        assert!(rows[0].locator.is_none());
        assert!(!rows[0].cells[1].editable);

        // Rows pending deletion are not editable.
        let mut stage = TableStage::default();
        stage.mark_delete(RowLocator {
            identity_values: vec![Value::Integer(1)],
        });
        let rows = view_rows(&result, 0, Some(&pk_identity()), Some(&stage));
        assert!(rows[0].deleted);
        assert!(rows[0].cells.iter().all(|c| !c.editable));
        assert!(rows[1].cells.iter().all(|c| c.editable));

        // Blob cells (by value) and blob-typed columns are read-only.
        let blob_result = QueryResult {
            columns: vec![
                crate::db::ColumnInfo { name: "id".into() },
                crate::db::ColumnInfo {
                    name: "cover".into(),
                },
            ],
            rows: vec![vec![Value::Integer(1), Value::Blob(vec![1, 2])]],
        };
        let rows = view_rows(&blob_result, 0, Some(&pk_identity()), None);
        assert!(!rows[0].cells[1].editable, "blob value cell");
        let null_blob = QueryResult {
            rows: vec![vec![Value::Integer(1), Value::Null]],
            ..blob_result
        };
        let rows = view_rows(&null_blob, 0, Some(&pk_identity()), None);
        assert!(
            rows[0].cells[1].editable,
            "row-level check passes for a NULL in a blob column…"
        );
        assert!(
            !cell_editable(&rows[0].cells[1], &kinds),
            "…but the blob-typed column blocks it"
        );
    }

    #[test]
    fn tab_order_steps_within_the_row_and_stops_at_the_edges() {
        let columns = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        assert_eq!(step_column(&columns, "a", 1), Some("b".into()));
        assert_eq!(step_column(&columns, "b", -1), Some("a".into()));
        assert_eq!(step_column(&columns, "c", 1), None);
        assert_eq!(step_column(&columns, "a", -1), None);
        assert_eq!(step_column(&columns, "missing", 1), None);
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
