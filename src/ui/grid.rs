use std::collections::{HashMap, HashSet};

use dioxus::prelude::*;

use crate::db::{
    detect_row_identity, ConnectionId, Dialect, ExportFormat, Filter, FilterOp, ForeignKeyMeta,
    Generated, Page, PageRequest, PreviewInfo, QueryResult, RowIdentity, RowLocator, SortDir,
    StagedChange, TableKind, TableMeta, Value, FETCH_CELL_MAX_BYTES,
};
use crate::util::human_bytes;

use super::editing::{editor_kind, CellEditor, EditNav, EditorKind};
use super::notice::{Banner, BannerKind, DelayedLoading, EmptyState};
use super::stage::{required_insert_columns, PendingInsert, TableStage};
use super::state::{AppState, ExportPane, ExportStatus, SchemaLoad, TableRef};

const PAGE_SIZE: u32 = 100;

/// Fixed data-row height in CSS pixels (FRE-32). Rows render one truncated
/// line at `text-xs` + `py-1`, so their height is uniform; pinning it lets the
/// windowed renderer compute exact scroll offsets and spacer heights. Measured
/// in the WebKitGTK webview (rows sit on a 33px pitch). The GridRow `<tr>` is
/// held to this height so an open inline editor can't shift the offsets.
const ROW_HEIGHT: f64 = 33.0;

/// Rows rendered above and below the viewport (FRE-32). A margin so a fast
/// flick rarely reveals a blank spacer before the range updates, and so small
/// errors from the sticky header offset never uncover an unrendered row.
const ROW_OVERSCAN: usize = 12;

/// Scroll/resize listener installed on `#dv-grid` (FRE-32). rAF-coalesces
/// bursts and reports `[scrollTop, clientHeight]` back over the eval channel so
/// the component can derive the visible row range. Installed once per mount.
const GRID_SCROLL_JS: &str = r#"
(() => {
  const el = document.getElementById('dv-grid');
  if (!el) return;
  let scheduled = false, settle = null;
  const report = () => { scheduled = false; dioxus.send([el.scrollTop, el.clientHeight]); };
  const onScroll = () => {
    if (!scheduled) { scheduled = true; requestAnimationFrame(report); }
    // Trailing settle: guarantee a final report at the resting position even
    // if momentum scrolling ends without a last 'scroll' event, so the window
    // can never stay stale (blank) after a fast fling.
    clearTimeout(settle);
    settle = setTimeout(report, 120);
  };
  el.addEventListener('scroll', onScroll, { passive: true });
  // Observe the grid element's own size instead of a window 'resize'
  // listener: a ResizeObserver is garbage-collected together with `el` when
  // the grid unmounts (a table switch remounts DataGrid), so it can't leak a
  // listener per opened table the way a never-removed window listener would.
  // It also catches layout-driven resizes (sidebar/pane), not just window
  // resizes.
  if (window.ResizeObserver) {
    new ResizeObserver(onScroll).observe(el);
  }
  requestAnimationFrame(report);
})();
"#;

/// The cell whose in-place editor is open, addressed by row key
/// ([`RowLocator::key`]) + column name. At most one editor is open per
/// grid.
#[derive(Debug, Clone, PartialEq)]
struct ActiveEdit {
    row_key: String,
    column: String,
}

/// What the value-expand popup shows: a short value already in hand, or a
/// truncated cell whose full value is loaded lazily via
/// [`AppState::load_cell`] (FRE-33).
#[derive(Debug, Clone, PartialEq)]
enum ExpandView {
    /// A value already fully in the page — rendered directly.
    Text(String),
    /// A truncated cell: fetch and show its full value.
    Fetch { locator: RowLocator, column: String },
}

/// Paged grid for one table: sortable headers, per-column contains/equals
/// filter, page navigation, row-count indicator, refresh — plus staged-edit
/// rendering (FRE-14): dirty cells and deletes are tinted, pending inserts
/// show as phantom rows, and a Save/Discard bar appears while the table's
/// stage is non-empty. Cells of editable rows open a type-aware in-place
/// editor (FRE-24, see [`CellEditor`]) on double-click or Enter; commits
/// only ever stage (never write through).
///
/// Row-level staging (FRE-25), all gated on the table being editable (a
/// usable row identity exists):
/// - "+ New row" under the grid appends a phantom [`InsertRow`] whose cells
///   all start as "database default" and open the same [`CellEditor`];
///   required columns (see [`required_insert_columns`]) are red-flagged and
///   block Save until filled. The phantom's ✕ removes it without staging.
/// - A leading checkbox column selects rows (header = select all on page);
///   "Delete N selected" stages deletes for the selection. Saving a stage
///   that contains deletes takes two clicks: the first arms an exact-count
///   confirmation ("Confirm: delete N + save M"), any staging activity
///   disarms it.
///
/// Callers key this component by table name, so all hook state here is
/// per-table and resets when another table is selected. (The refresh nonce
/// is NOT local: it lives in [`AppState::grid_refresh`] so a successful save
/// can force a refetch from outside the component.)
#[component]
pub fn DataGrid(id: ConnectionId, table: TableRef) -> Element {
    let state = use_context::<AppState>();
    let mut page = use_signal(|| 0u64);
    // Which cell's editor is open (by row key + column).
    let mut editing = use_signal(|| Option::<ActiveEdit>::None);
    // Keyboard focus ring in the grid (row, col into the visible page's
    // rows × columns), and the value-expand popup (FRE-15).
    let mut focused_cell = use_signal(|| Option::<(usize, usize)>::None);
    // The value-expand popup (FRE-15): either an already-known short value
    // rendered inline, or a truncated cell whose full value is fetched on
    // demand (FRE-33).
    let mut expanded = use_signal(|| Option::<ExpandView>::None);
    let mut sort = use_signal(|| Option::<(String, SortDir)>::None);
    // Windowed rendering (FRE-32): the grid's live scroll offset and viewport
    // height, fed by a scroll/resize listener installed on `#dv-grid`. The
    // visible row range is derived from these; only rows in that range (plus
    // overscan) are put in the DOM. `viewport_h` seeds non-zero so the first
    // render before the listener reports still windows to a sane range.
    let mut scroll_top = use_signal(|| 0.0f64);
    let mut viewport_h = use_signal(|| 600.0f64);
    // Rows ticked for deletion (row key → locator). UI-only state: nothing
    // is staged until "Delete N selected".
    let mut selected = use_signal(HashMap::<String, RowLocator>::new);
    // The armed save confirmation: the exact change list shown to the user
    // when the Save button turned into "Confirm: delete N…". The second
    // click only proceeds while the stage still generates this exact list.
    let mut confirm = use_signal(|| Option::<Vec<StagedChange>>::None);
    // Once armed, the FIRST divergence of the stage from the snapshot
    // permanently disarms — clearing `confirm`, not just hiding it. Without
    // this, staging then un-staging back to an identical change list (e.g.
    // add a phantom insert, then remove it) would silently re-arm the
    // confirmation, letting the next single click save without a fresh
    // confirm step. Requiring a new first click after any change keeps the
    // "two clicks per confirmation instance" guarantee.
    let confirm_reset_table = table.clone();
    use_effect(move || {
        let Some(snapshot) = confirm.read().clone() else {
            return;
        };
        let current = state
            .table_stage(id, &confirm_reset_table)
            .map(|s| s.changes())
            .unwrap_or_default();
        if current != snapshot {
            confirm.set(None);
        }
    });
    // The filter inputs are staged locally and only hit the query when
    // applied, so typing doesn't fire a query per keystroke.
    let mut filter_column = use_signal(String::new);
    let mut filter_op = use_signal(|| FilterOp::Contains);
    let mut filter_text = use_signal(String::new);
    // Seed the filter from a pending foreign-key focus targeting this table
    // (FRE-29), so a jump paints the referenced row with no unfiltered flash.
    // The consuming effect below clears the focus (and handles a same-table
    // jump, where the grid stays mounted and only the filter changes).
    let seed_table = table.clone();
    let mut applied_filter = use_signal(move || {
        state
            .pending_focus
            .peek()
            .get(&id)
            .filter(|focus| focus.table == seed_table)
            .and_then(|focus| focus.filter.clone())
    });
    let focus_table = table.clone();
    use_effect(move || {
        let mut state = state;
        let matched = state
            .pending_focus
            .read()
            .get(&id)
            .filter(|focus| focus.table == focus_table)
            .cloned();
        if let Some(focus) = matched {
            applied_filter.set(focus.filter);
            page.set(0);
            // Reading pending_focus above subscribes this effect; removing the
            // consumed entry re-runs it once (now no match) — no loop.
            state.pending_focus.write().remove(&id);
        }
    });

    // Close any open editor and drop the row selection when the rows change
    // under them: a page flip, sort/filter change, or refetch replaces the
    // grid's contents — a stale ActiveEdit would spontaneously re-open the
    // editor if its row key scrolled back into view, and a stale selection
    // could stage deletes for rows the user no longer sees.
    let table_key_for_reset = table.key();
    use_effect(move || {
        let _ = page();
        let _ = sort.read();
        let _ = applied_filter.read();
        let _ = state
            .grid_refresh
            .read()
            .get(&(id, table_key_for_reset.clone()));
        editing.set(None);
        selected.set(HashMap::new());
        // Re-seed the focus ring at the first cell and drop any expand popup;
        // the clamp effect below trims it to None for an empty page.
        focused_cell.set(Some((0, 0)));
        expanded.set(None);
        // Reset the windowed scroll position too (FRE-32) so a new page/sort/
        // filter starts at the top, both the tracked offset and the container.
        scroll_top.set(0.0);
        document::eval(
            "requestAnimationFrame(() => { \
                const el = document.getElementById('dv-grid'); \
                if (el) el.scrollTop = 0; \
            });",
        );
    });

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
        //
        // The bounded fetch also needs the table's columns (to preview large
        // ones) and the columns that must NOT be previewed: identity keys and
        // foreign-key columns, whose truncation would misaddress rows or
        // misdirect a jump (FRE-33).
        let (columns, no_preview, extra_key_column) = {
            let dialect = state.registry.read().get(id).map(|c| c.pool.dialect());
            let schemas = state.schemas.read();
            let meta = find_table(schemas.get(&id), &table);
            match (meta, dialect) {
                (Some(meta), Some(dialect)) => {
                    let identity = detect_row_identity(meta, dialect);
                    let extra = match &identity {
                        Some(RowIdentity::Rowid { column }) => Some(column.clone()),
                        _ => None,
                    };
                    let mut no_preview: Vec<String> = identity
                        .as_ref()
                        .map(|i| i.key_columns().iter().map(|s| s.to_string()).collect())
                        .unwrap_or_default();
                    for fk in &meta.foreign_keys {
                        no_preview.extend(fk.columns.iter().cloned());
                    }
                    (meta.columns.clone(), no_preview, extra)
                }
                _ => (Vec::new(), Vec::new(), None),
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
            let no_preview_refs: Vec<&str> = no_preview.iter().map(String::as_str).collect();
            let page = pool
                .fetch_page_bounded(&request, &columns, &no_preview_refs)
                .await?;
            Ok::<(Page, Option<String>), crate::db::DbError>((page, request.extra_key_column))
        }
    });

    // Row count, fetched separately from the page so it is NOT re-run on every
    // page flip or sort change (FRE-40): `COUNT(*)` depends only on the table
    // and filter. It re-runs when the filter changes or a write bumps
    // `grid_refresh`; between those, Dioxus keeps the resolved value, so
    // flipping pages reuses the cached count with no extra query.
    let count_table = table.clone();
    let count_resource = use_resource(move || {
        let table = count_table.clone();
        let request = PageRequest {
            schema: table.schema.clone(),
            table: table.name.clone(),
            limit: PAGE_SIZE,
            offset: 0,
            sort: None, // COUNT(*) ignores ordering
            filter: applied_filter(),
            extra_key_column: None,
        };
        let _ = state.grid_refresh.read().get(&(id, table.key())).copied();
        let pool = state.registry.read().get(id).map(|c| c.pool.clone());
        async move {
            let Some(pool) = pool else {
                return Err(crate::db::DbError::Query("connection closed".into()));
            };
            pool.count_rows(&request).await
        }
    });

    // Clamp the page if the row count shrank below the current offset — e.g. a
    // delete or an external write (via Refresh) leaves the table with fewer
    // pages than the one being viewed. Without this the footer reads
    // "rows 301–300 of 250" and the page sits empty (FRE-42). Only acts on a
    // resolved count; setting the page re-runs this once with a now-valid page,
    // so it settles without looping. (Reactive; verified live, not by unit test.)
    use_effect(move || {
        if *count_resource.state().read() == UseResourceState::Pending {
            return;
        }
        let total = match count_resource.read().as_ref() {
            Some(Ok(total)) => *total,
            _ => return,
        };
        let last_page = total.saturating_sub(1) / PAGE_SIZE as u64;
        if page() > last_page {
            page.set(last_page);
        }
    });

    // Keyboard-navigation model of the visible page (FRE-15): recomputed with
    // the fetched page and the stage, read by the grid container's key handler
    // for focus movement and Enter. Mirrors the render's row prep (view_rows)
    // but owns its data so the `'static` key closure can read it off-render.
    let nav_table = table.clone();
    let grid_nav = use_memo(move || {
        let current = rows_resource.read();
        let Some(Ok((page, extra_key))) = current.as_ref() else {
            return GridNav::default();
        };
        let result = &page.result;
        let dialect = state.registry.read().get(id).map(|c| c.pool.dialect());
        let table_meta = find_table(state.schemas.read().get(&id), &nav_table).cloned();
        let identity = match (&table_meta, dialect) {
            (Some(meta), Some(dialect)) => detect_row_identity(meta, dialect),
            _ => None,
        };
        let column_kinds = column_kinds_of(table_meta.as_ref());
        let stage = state.table_stage(id, &nav_table);
        let hidden = usize::from(extra_key.is_some());
        let headers: Vec<String> = if result.columns.is_empty() {
            table_meta
                .as_ref()
                .map(|t| t.columns.iter().map(|c| c.name.clone()).collect())
                .unwrap_or_default()
        } else {
            result
                .columns
                .iter()
                .skip(hidden)
                .map(|c| c.name.clone())
                .collect()
        };
        let rows = view_rows(
            result,
            &page.previews,
            hidden,
            identity.as_ref(),
            stage.as_ref(),
        );
        GridNav::build(headers, &rows, &column_kinds)
    });

    // Keep the focus ring inside the current page and seed it once data
    // arrives, so it is visible and never indexes out of range after a page or
    // filter change shrinks the grid.
    use_effect(move || {
        let (rows, cols) = grid_nav.read().dims();
        if rows == 0 || cols == 0 {
            if focused_cell.peek().is_some() {
                focused_cell.set(None);
            }
        } else {
            let (r, c) = focused_cell.peek().unwrap_or((0, 0));
            let clamped = (r.min(rows - 1), c.min(cols - 1));
            if *focused_cell.peek() != Some(clamped) {
                focused_cell.set(Some(clamped));
            }
        }
    });

    // Scroll the focused cell into view as it moves (FRE-15 + FRE-32). With
    // windowed rows an offscreen focused row may not be in the DOM yet, so we
    // can't rely on its node: instead scroll the container to the focused
    // row's computed offset (rows are `ROW_HEIGHT` tall), which also updates
    // the visible range so the row renders. A second frame then nudges the
    // now-rendered cell into view horizontally (column offsets aren't fixed).
    use_effect(move || {
        let Some((r, _c)) = *focused_cell.read() else {
            return;
        };
        let top = r as f64 * ROW_HEIGHT;
        document::eval(&format!(
            "requestAnimationFrame(() => {{ \
                const el = document.getElementById('dv-grid'); \
                if (!el) return; \
                const h = {ROW_HEIGHT}; const top = {top}; const pad = 2 * h; \
                if (top < el.scrollTop + pad) {{ \
                    el.scrollTop = Math.max(0, top - pad); \
                }} else if (top + h > el.scrollTop + el.clientHeight - pad) {{ \
                    el.scrollTop = top + h - el.clientHeight + pad; \
                }} \
                requestAnimationFrame(() => {{ \
                    const c = document.getElementById('dv-focused-cell'); \
                    if (c) c.scrollIntoView({{ block: 'nearest', inline: 'nearest' }}); \
                }}); \
            }});",
        ));
    });

    // Install a scroll/resize listener on the grid container once per mount and
    // pump the offset/height it reports into `scroll_top`/`viewport_h`, which
    // drive the visible-row range (FRE-32). rAF-coalesced so a scroll burst
    // yields at most one update per frame. The channel stays open for the
    // component's life; it reads no signals, so it installs exactly once.
    use_effect(move || {
        spawn(async move {
            let mut channel = document::eval(GRID_SCROLL_JS);
            while let Ok(msg) = channel.recv::<(f64, f64)>().await {
                let (top, height) = msg;
                if *scroll_top.peek() != top {
                    scroll_top.set(top);
                }
                if height > 0.0 && *viewport_h.peek() != height {
                    viewport_h.set(height);
                }
            }
        });
    });

    // The rows to actually put in the DOM (FRE-32): a contiguous window around
    // the viewport. Arrow-keying to an offscreen row is handled by the
    // focus-scroll effect below, which scrolls that row into view (updating
    // `scroll_top`, so this window then includes it) rather than widening the
    // window to the focus — the latter would drag the window back to a
    // now-offscreen focus and defeat windowing after a mouse scroll.
    let visible_range = use_memo(move || {
        let total = grid_nav.read().rows.len();
        compute_visible_range(scroll_top(), viewport_h(), ROW_HEIGHT, total, ROW_OVERSCAN)
    });

    // Return keyboard focus to the grid container whenever no cell editor is
    // open — on mount, and after an editor closes (the editor input, not the
    // container, held focus) — so arrow navigation keeps working without a
    // mouse click. Only fires on an editing → None transition, so it never
    // steals focus from the filter box or sidebar while the grid is idle.
    use_effect(move || {
        if editing.read().is_none() {
            document::eval(
                "requestAnimationFrame(() => { \
                    const el = document.getElementById('dv-grid'); \
                    if (el) el.focus(); \
                });",
            );
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
    // Foreign keys of this table drive the clickable FK cells (FRE-29).
    // `col_to_fk` maps a referencing column to the index of the FK it belongs
    // to; a column in several FKs takes the first (documented v1 limit).
    let foreign_keys: Vec<ForeignKeyMeta> = table_meta
        .as_ref()
        .map(|t| t.foreign_keys.clone())
        .unwrap_or_default();
    let col_to_fk: HashMap<String, usize> = {
        let mut map = HashMap::new();
        for (index, fk) in foreign_keys.iter().enumerate() {
            for column in &fk.columns {
                map.entry(column.clone()).or_insert(index);
            }
        }
        map
    };
    // Whether the FK Back stack has anywhere to return to (reactive).
    let can_back = state.can_go_back(id);

    let dialect: Option<Dialect> = state.registry.read().get(id).map(|c| c.pool.dialect());
    let identity: Option<RowIdentity> = match (&table_meta, dialect) {
        (Some(meta), Some(dialect)) => detect_row_identity(meta, dialect),
        _ => None,
    };
    // Per-column editor kind + nullability, from introspection. Cells whose
    // column is missing here (transient schema/result mismatch) fall back
    // to a plain text editor on a nullable column.
    let column_kinds: HashMap<String, (EditorKind, bool)> = column_kinds_of(table_meta.as_ref());
    // Editing consults the same identity detection: no identity → no
    // editors; the notice explains up front why the rows are read-only.
    let read_only_notice: Option<&'static str> = match (&table_meta, identity.is_none()) {
        (Some(meta), true) => {
            if meta.kind == TableKind::MaterializedView {
                Some("Materialized views are read-only.")
            } else if meta.kind == TableKind::View {
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
    // Insert/delete affordances only exist where editing works at all.
    let select_enabled = identity.is_some();
    // Required-column flagging for pending inserts: NOT NULL + no default +
    // not auto-assigned (see required_insert_columns for the per-backend
    // rules). Unfilled required cells red-flag and block Save.
    let required: HashSet<String> = match (&table_meta, dialect) {
        (Some(meta), Some(dialect)) => required_insert_columns(meta, dialect),
        _ => HashSet::new(),
    };
    let missing_required = stage
        .as_ref()
        .map(|s| s.missing_required(&required))
        .unwrap_or(0);
    let delete_count = stage.as_ref().map(TableStage::delete_count).unwrap_or(0);
    // The confirmation stays armed only while its snapshot still matches
    // the stage exactly (see `confirm` above).
    let armed = delete_count > 0
        && stage
            .as_ref()
            .is_some_and(|s| confirm.read().as_deref() == Some(s.changes().as_slice()));
    // The two-step navigation guard parks blocked navigations here; the Save
    // bar explains how to proceed (see AppState::nav_guard for the UX).
    let nav_blocked = state
        .nav_guard
        .read()
        .as_ref()
        .is_some_and(|nav| nav.id == id);

    let current = rows_resource.read();
    let sort_value = sort();
    let export_status: Option<ExportStatus> = state
        .export_status
        .read()
        .get(&(id, ExportPane::Grid))
        .cloned();
    let export_table = table.clone();
    let refresh_table = table.clone();
    let save_table = table.clone();
    let discard_table = table.clone();
    let delete_table = table.clone();
    let row_table = table.clone();

    // Grid keyboard navigation (FRE-15). Attached to the focusable scroll
    // container, so it also receives keydowns bubbling from focused children;
    // it no-ops while an editor is open (whose own keys bubble here). Movement
    // and Enter act on `grid_nav`/`focused_cell`; PageUp/PageDown flip pages.
    let on_grid_key = move |evt: KeyboardEvent| {
        if editing.peek().is_some() {
            return;
        }
        let code = evt.code();
        // Escape closes the value-expand popup, if one is open.
        if code == Code::Escape {
            if expanded.peek().is_some() {
                evt.prevent_default();
                expanded.set(None);
            }
            return;
        }
        let (rows, cols) = grid_nav.peek().dims();
        if rows == 0 || cols == 0 {
            return;
        }
        let pos = focused_cell.peek().unwrap_or((0, 0));
        if code == Code::Enter || code == Code::NumpadEnter {
            evt.prevent_default();
            let nav = grid_nav.peek();
            let (r, c) = (pos.0.min(rows - 1), pos.1.min(cols - 1));
            if let Some(cell) = nav.rows.get(r).and_then(|row| row.cells.get(c)) {
                // Editable cell → open the in-place editor; otherwise expand
                // the value. A truncated cell fetches its full value on demand
                // (FRE-33); a complete one shows the in-hand text.
                match (cell.editable, nav.rows[r].key.clone()) {
                    (true, Some(key)) => editing.set(Some(ActiveEdit {
                        row_key: key,
                        column: cell.column.clone(),
                    })),
                    _ => {
                        let view = match (cell.truncated, nav.rows[r].locator.clone()) {
                            (true, Some(locator)) => ExpandView::Fetch {
                                locator,
                                column: cell.column.clone(),
                            },
                            _ => ExpandView::Text(cell.display.clone()),
                        };
                        expanded.set(Some(view));
                    }
                }
            }
            return;
        }
        let Some(mv) = grid_move_for(code, evt.modifiers().ctrl()) else {
            return;
        };
        evt.prevent_default();
        match apply_grid_move(pos, mv, rows, cols) {
            FocusOutcome::Cell(next) => focused_cell.set(Some(next)),
            FocusOutcome::PrevPage => {
                let p = *page.peek();
                if p > 0 {
                    page.set(p - 1);
                }
            }
            FocusOutcome::NextPage => {
                // Don't page past the end (mirrors the Next button's guard).
                // A re-running count reads as 0 → blocked, like the footer.
                let total = if *count_resource.state().peek() == UseResourceState::Pending {
                    0
                } else {
                    match count_resource.peek().as_ref() {
                        Some(Ok(total)) => *total,
                        _ => 0,
                    }
                };
                let p = *page.peek();
                let last = p * PAGE_SIZE as u64 + rows as u64;
                if last < total {
                    page.set(p + 1);
                }
            }
        }
    };

    rsx! {
        div { class: "flex h-full min-h-0 flex-col",
            // Filter bar
            div { class: "flex items-center gap-2 border-b border-slate-200 dark:border-slate-800 px-3 py-2 text-sm",
                // Back: return to the view a foreign-key jump came from (FRE-29).
                if can_back {
                    button {
                        class: "rounded px-2 py-1 text-xs text-cyan-700 dark:text-cyan-300 hover:bg-cyan-100 dark:hover:bg-cyan-950/40",
                        title: "Back to the previous view",
                        onclick: move |_| state.navigate_back(id),
                        "← Back"
                    }
                }
                select {
                    class: "rounded border border-slate-300 dark:border-slate-700 bg-slate-100 dark:bg-slate-950 px-2 py-1 text-xs text-slate-900 dark:text-slate-300",
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
                    class: "rounded border border-slate-300 dark:border-slate-700 bg-slate-100 dark:bg-slate-950 px-2 py-1 text-xs text-slate-900 dark:text-slate-300",
                    onchange: move |evt| {
                        filter_op.set(if evt.value() == "equals" { FilterOp::Equals } else { FilterOp::Contains });
                    },
                    option { value: "contains", "contains" }
                    option { value: "equals", "equals" }
                }
                input {
                    // `dv-filter` is the target of the `/` focus shortcut (FRE-15).
                    id: "dv-filter",
                    class: "w-48 rounded border border-slate-300 dark:border-slate-700 bg-slate-100 dark:bg-slate-950 px-2 py-1 font-mono text-xs text-slate-900 dark:text-slate-200 placeholder:text-slate-400 dark:placeholder:text-slate-600",
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
                    class: "rounded bg-slate-300 dark:bg-slate-700 px-3 py-1 text-xs text-slate-900 dark:text-slate-100 hover:bg-slate-400 dark:hover:bg-slate-600",
                    onclick: move |_| apply_filter(filter_column, filter_op, filter_text, applied_filter, page),
                    "Apply"
                }
                // An FK jump installs an equality filter the single-column
                // inputs can't show; a chip spells out what's pinned (FRE-29).
                if let Some(description) = equality_filter_label(applied_filter.read().as_ref()) {
                    span {
                        class: "rounded bg-cyan-100 dark:bg-cyan-950/50 px-2 py-1 font-mono text-xs text-cyan-700 dark:text-cyan-300",
                        title: "Filtered to a referenced row",
                        "{description}"
                    }
                }
                if applied_filter.read().is_some() {
                    button {
                        class: "rounded px-2 py-1 text-xs text-slate-500 dark:text-slate-400 hover:text-slate-900 dark:hover:text-slate-100",
                        onclick: move |_| {
                            applied_filter.set(None);
                            filter_text.set(String::new());
                            page.set(0);
                        },
                        "Clear"
                    }
                }
                div { class: "flex-1" }
                if let Some(status) = export_status.as_ref() {
                    {
                        let (text, class) = status.line();
                        rsx! { span { class: "truncate text-xs {class}", title: "{text}", "{text}" } }
                    }
                }
                if let Some(export_dialect) = dialect {
                    button {
                        class: "rounded px-2 py-1 text-xs text-slate-500 dark:text-slate-400 hover:bg-slate-200 dark:hover:bg-slate-800 hover:text-slate-900 dark:hover:text-slate-100",
                        title: "Export the current view (filter + sort, all rows) to CSV",
                        onclick: {
                            let export_table = export_table.clone();
                            move |_| spawn_grid_export(state, id, export_table.clone(), export_dialect, sort.peek().clone(), applied_filter.peek().clone(), ExportFormat::Csv)
                        },
                        "Export CSV"
                    }
                    button {
                        class: "rounded px-2 py-1 text-xs text-slate-500 dark:text-slate-400 hover:bg-slate-200 dark:hover:bg-slate-800 hover:text-slate-900 dark:hover:text-slate-100",
                        title: "Export the current view (filter + sort, all rows) to JSON",
                        onclick: {
                            let export_table = export_table.clone();
                            move |_| spawn_grid_export(state, id, export_table.clone(), export_dialect, sort.peek().clone(), applied_filter.peek().clone(), ExportFormat::Json)
                        },
                        "Export JSON"
                    }
                }
                button {
                    class: "rounded px-2 py-1 text-xs text-slate-500 dark:text-slate-400 hover:bg-slate-200 dark:hover:bg-slate-800 hover:text-slate-900 dark:hover:text-slate-100",
                    title: "Re-run the current query",
                    onclick: move |_| state.bump_grid_refresh(id, &refresh_table.key()),
                    "↻ Refresh"
                }
            }
            // Save/Discard bar: appears while this table has staged changes.
            if pending_count > 0 {
                div { class: "flex items-center gap-3 border-b border-amber-300 dark:border-amber-700/50 bg-amber-100 dark:bg-amber-950/40 px-3 py-1.5 text-xs",
                    span { class: "font-semibold text-amber-700 dark:text-amber-300",
                        if pending_count == 1 { "1 pending change" } else { "{pending_count} pending changes" }
                    }
                    if nav_blocked {
                        span { class: "text-amber-700 dark:text-amber-200",
                            "Unsaved changes — Save or Discard first (repeat the action to discard & leave)."
                        }
                    }
                    if armed {
                        span { class: "text-red-600 dark:text-red-300",
                            "{confirm_notice(delete_count, pending_count - delete_count)}"
                        }
                    }
                    if let Some(message) = required_missing_message(missing_required) {
                        span { class: "text-red-600 dark:text-red-300", "{message}" }
                    }
                    if let Some(error) = save_error {
                        span { class: "min-w-0 flex-1 truncate text-red-600 dark:text-red-400", title: "{error}", "{error}" }
                    } else {
                        div { class: "flex-1" }
                    }
                    button {
                        class: if armed {
                            "rounded bg-red-700 px-3 py-1 font-semibold text-white hover:bg-red-600 disabled:opacity-40"
                        } else {
                            "rounded bg-emerald-700 px-3 py-1 font-semibold text-white hover:bg-emerald-600 disabled:opacity-40"
                        },
                        disabled: save_disabled(saving, missing_required),
                        // Two-step save when deletes are staged: the first
                        // click arms the exact-count confirmation, the second
                        // (with the stage unchanged) saves. Stages without
                        // deletes save immediately.
                        onclick: move |_| {
                            let Some(stage) = state.table_stage(id, &save_table) else { return };
                            if stage.saving {
                                return;
                            }
                            let changes = stage.changes();
                            if stage.delete_count() > 0 && confirm.peek().as_deref() != Some(changes.as_slice()) {
                                confirm.set(Some(changes));
                                return;
                            }
                            confirm.set(None);
                            state.save_staged(id, &save_table);
                        },
                        "{save_button_label(saving, armed, delete_count, pending_count - delete_count)}"
                    }
                    button {
                        class: "rounded border border-slate-400 dark:border-slate-600 px-3 py-1 text-slate-900 dark:text-slate-300 hover:bg-slate-200 dark:hover:bg-slate-800",
                        disabled: saving,
                        onclick: move |_| {
                            confirm.set(None);
                            state.discard_staged(id, &discard_table);
                        },
                        "Discard"
                    }
                }
            }
            // Selection bar: rows ticked for deletion (nothing staged yet).
            if !selected.read().is_empty() {
                div { class: "flex items-center gap-3 border-b border-red-300 dark:border-red-900/50 bg-red-100 dark:bg-red-950/30 px-3 py-1.5 text-xs",
                    span { class: "text-red-600 dark:text-red-200",
                        if selected.read().len() == 1 { "1 row selected" } else { "{selected.read().len()} rows selected" }
                    }
                    div { class: "flex-1" }
                    button {
                        class: "rounded bg-red-800 px-3 py-1 font-semibold text-white hover:bg-red-700",
                        onclick: move |_| {
                            // Stage deletes for the whole selection, then
                            // clear it — the rows render red (pending
                            // delete) from the stage now.
                            let locators = selection_locators(&selected.peek());
                            for locator in locators {
                                state.stage_delete(id, &delete_table, locator);
                            }
                            selected.set(HashMap::new());
                        },
                        "Delete {selected.read().len()} selected"
                    }
                    button {
                        class: "rounded border border-slate-400 dark:border-slate-600 px-3 py-1 text-slate-900 dark:text-slate-300 hover:bg-slate-200 dark:hover:bg-slate-800",
                        onclick: move |_| selected.set(HashMap::new()),
                        "Clear selection"
                    }
                }
            }
            // Read-only notice (views / no usable row key)
            if let Some(notice) = read_only_notice {
                div { class: "px-3 pt-2",
                    Banner { kind: BannerKind::Info, message: notice.to_string() }
                }
            }
            // Grid — a single focusable region (tabindex 0) so arrow-key cell
            // navigation works without per-cell tab stops (FRE-15). Focused on
            // mount so the ring responds immediately; `outline-none` since the
            // ring itself signals focus.
            div {
                id: "dv-grid",
                class: "min-h-0 flex-1 overflow-auto outline-none",
                tabindex: "0",
                onkeydown: on_grid_key,
                match current.as_ref() {
                    None => rsx! {
                        DelayedLoading { label: "Loading…" }
                    },
                    Some(Err(err)) => rsx! {
                        div { class: "p-3",
                            Banner { kind: BannerKind::Error, message: err.to_string() }
                        }
                    },
                    Some(Ok((page_data, extra_key))) => {
                        let result = &page_data.result;
                        // The fetch prepended the row-identity key column
                        // (rowid) when one was requested; keep it for
                        // locators, hide it from display.
                        let hidden = usize::from(extra_key.is_some());
                        let headers: Vec<String> = if result.columns.is_empty() {
                            schema_columns.clone()
                        } else {
                            result.columns.iter().skip(hidden).map(|c| c.name.clone()).collect()
                        };
                        let rows = view_rows(result, &page_data.previews, hidden, identity.as_ref(), stage.as_ref());
                        let pending_inserts: Vec<PendingInsert> = stage
                            .as_ref()
                            .map(|s| s.inserts().to_vec())
                            .unwrap_or_default();
                        // This page's selectable rows (addressable and not
                        // already pending delete), for select-all-on-page.
                        let selectable: Vec<(String, RowLocator)> = rows
                            .iter()
                            .filter(|r| !r.deleted)
                            .filter_map(|r| Some((r.key.clone()?, r.locator.clone()?)))
                            .collect();
                        let all_selected = !selectable.is_empty() && {
                            let sel = selected.read();
                            selectable.iter().all(|(key, _)| sel.contains_key(key))
                        };
                        let insert_headers = headers.clone();
                        let insert_parent_table = row_table.clone();
                        let new_row_table = row_table.clone();
                        let empty = empty_state(
                            rows.is_empty() && pending_inserts.is_empty(),
                            applied_filter.read().is_some(),
                        );
                        // Windowed rendering (FRE-32): only rows in the visible
                        // range go in the DOM; a top and bottom spacer row of
                        // the elided rows' total height keeps the scrollbar and
                        // offsets correct. `total_cols` sizes the spacers'
                        // single colspan cell across every column.
                        let total_rows = rows.len();
                        let total_cols = headers.len() + usize::from(select_enabled);
                        let (win_start, win_end) = *visible_range.read();
                        let (win_start, win_end) = (win_start.min(total_rows), win_end.min(total_rows));
                        let top_spacer = win_start as f64 * ROW_HEIGHT;
                        let bottom_spacer = (total_rows - win_end) as f64 * ROW_HEIGHT;
                        let windowed_rows: Vec<(usize, RowView)> = rows
                            .into_iter()
                            .enumerate()
                            .filter(|(index, _)| *index >= win_start && *index < win_end)
                            .collect();
                        rsx! {
                            match empty {
                                // No-filter-match: distinct from an empty
                                // table, with a Clear-filter action.
                                Some(GridEmpty::NoMatch) => rsx! {
                                    EmptyState {
                                        icon: "\u{1F50D}", // 🔍 magnifying glass
                                        title: "No rows match the filter",
                                        hint: "No rows in this table match the current filter.",
                                        button {
                                            class: "rounded border border-slate-300 dark:border-slate-700 px-3 py-1 text-xs text-slate-700 dark:text-slate-300 hover:bg-slate-200 dark:hover:bg-slate-800",
                                            onclick: move |_| {
                                                applied_filter.set(None);
                                                filter_text.set(String::new());
                                                page.set(0);
                                            },
                                            "Clear filter"
                                        }
                                    }
                                },
                                // Empty table: the "+ New row" affordance below
                                // stays available for editable tables.
                                Some(GridEmpty::Table) => rsx! {
                                    EmptyState {
                                        icon: "\u{1F4C4}", // 📄 page
                                        title: "This table has no rows",
                                        hint: "There is no data here yet.",
                                    }
                                },
                                None => rsx! {
                                table { class: "w-full border-collapse text-left",
                                    thead { class: "sticky top-0 bg-slate-100 dark:bg-slate-900",
                                        tr {
                                            if select_enabled {
                                                th { class: "w-8 border-b border-slate-300 dark:border-slate-700 px-2 py-1.5",
                                                    input {
                                                        r#type: "checkbox",
                                                        class: "accent-red-500",
                                                        title: "Select all rows on this page",
                                                        checked: all_selected,
                                                        oninput: move |_| {
                                                            let currently_all = !selectable.is_empty() && {
                                                                let sel = selected.peek();
                                                                selectable.iter().all(|(key, _)| sel.contains_key(key))
                                                            };
                                                            if currently_all {
                                                                selected.set(HashMap::new());
                                                            } else {
                                                                let mut map = selected.peek().clone();
                                                                for (key, locator) in &selectable {
                                                                    map.insert(key.clone(), locator.clone());
                                                                }
                                                                selected.set(map);
                                                            }
                                                        },
                                                    }
                                                }
                                            }
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
                                        // Top spacer: the height of the rows
                                        // elided above the window (FRE-32).
                                        if top_spacer > 0.0 {
                                            tr {
                                                td {
                                                    colspan: "{total_cols}",
                                                    style: "height:{top_spacer}px;padding:0;border:0;",
                                                }
                                            }
                                        }
                                        for (index, row) in windowed_rows {
                                            GridRow {
                                                key: "{row_render_key(&row, index)}",
                                                id,
                                                table: row_table.clone(),
                                                row,
                                                column_kinds: column_kinds.clone(),
                                                foreign_keys: foreign_keys.clone(),
                                                col_to_fk: col_to_fk.clone(),
                                                dialect: dialect.unwrap_or(Dialect::Sqlite),
                                                editing,
                                                // The focused column in this row (FRE-15), else None; only the
                                                // two rows whose focus changed re-render on a move.
                                                focused_col: match focused_cell() {
                                                    Some((r, c)) if r == index => Some(c),
                                                    _ => None,
                                                },
                                                select_enabled,
                                                selected,
                                                on_fk_jump: {
                                                    let origin = row_table.clone();
                                                    move |(fk, row_values): (ForeignKeyMeta, HashMap<String, Value>)| {
                                                        state.navigate_fk(id, &fk, &row_values, &origin, applied_filter.peek().clone());
                                                    }
                                                },
                                            }
                                        }
                                        // Bottom spacer: the height of the rows
                                        // elided below the window (FRE-32).
                                        if bottom_spacer > 0.0 {
                                            tr {
                                                td {
                                                    colspan: "{total_cols}",
                                                    style: "height:{bottom_spacer}px;padding:0;border:0;",
                                                }
                                            }
                                        }
                                        // Pending inserts: phantom rows with
                                        // editable "database default" cells.
                                        for insert in pending_inserts {
                                            InsertRow {
                                                key: "{insert.row_key()}",
                                                id,
                                                table: insert_parent_table.clone(),
                                                insert,
                                                headers: insert_headers.clone(),
                                                column_kinds: column_kinds.clone(),
                                                required: required.clone(),
                                                dialect: dialect.unwrap_or(Dialect::Sqlite),
                                                lead_cell: select_enabled,
                                                editing,
                                            }
                                        }
                                    }
                                }
                                },
                            }
                            // Insert affordance (editable tables only):
                            // appends a phantom row, all columns defaulted.
                            if select_enabled {
                                button {
                                    class: "m-2 rounded border border-dashed border-emerald-300 dark:border-emerald-700/70 px-3 py-1 \
                                            text-xs text-emerald-700 dark:text-emerald-300 hover:bg-emerald-100 dark:hover:bg-emerald-950/40",
                                    onclick: move |_| state.stage_insert_row(id, &new_row_table),
                                    "+ New row"
                                }
                            }
                        }
                    }
                }
            }
            // Footer: paging + counts
            div { class: "flex items-center gap-3 border-t border-slate-200 dark:border-slate-800 px-3 py-1.5 text-xs text-slate-500 dark:text-slate-400",
                match current.as_ref() {
                    Some(Ok((page_data, _))) => {
                        let result = &page_data.result;
                        // The count comes from its own resource (FRE-40); it
                        // stays resolved across page flips, so flipping reuses
                        // it. While it is *re-running* (a filter change or a
                        // write bump — `use_resource` keeps the old value but
                        // flips its state to Pending), treat the total as
                        // unknown so we never pair new rows with a stale count.
                        let total = if *count_resource.state().read() == UseResourceState::Pending {
                            None
                        } else {
                            count_resource
                                .read()
                                .as_ref()
                                .and_then(|r| r.as_ref().ok().copied())
                        };
                        let loaded = result.rows.len() as u64;
                        let first = if loaded == 0 { 0 } else { page() * PAGE_SIZE as u64 + 1 };
                        let last = page() * PAGE_SIZE as u64 + loaded;
                        // While the total is unknown, don't offer Next (avoid
                        // paging past the end); a full page implies there may be
                        // more, so the ellipsis reads honestly.
                        let at_end = total.map(|t| last >= t).unwrap_or(true);
                        rsx! {
                            match total {
                                Some(total) => rsx! { span { "rows {first}–{last} of {total}" } },
                                None => rsx! { span { "rows {first}–{last} of …" } },
                            }
                            div { class: "flex-1" }
                            button {
                                class: "rounded px-2 py-0.5 hover:bg-slate-200 dark:hover:bg-slate-800 disabled:opacity-40",
                                disabled: page() == 0,
                                onclick: move |_| { let p = page(); page.set(p.saturating_sub(1)); },
                                "← Prev"
                            }
                            span { "page {page() + 1}" }
                            button {
                                class: "rounded px-2 py-0.5 hover:bg-slate-200 dark:hover:bg-slate-800 disabled:opacity-40",
                                disabled: at_end,
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
            // Value-expand popup (FRE-15): Enter on a read-only / non-editable
            // focused cell shows its full value — fetched on demand for a
            // truncated cell (FRE-33). Dismissed by a backdrop click, the ✕,
            // or Escape (handled by the grid container).
            if let Some(view) = expanded.read().clone() {
                div {
                    class: "fixed inset-0 z-40 flex items-center justify-center bg-black/40 p-4",
                    onclick: move |_| expanded.set(None),
                    div {
                        class: "max-h-[70vh] w-full max-w-2xl overflow-auto rounded-lg border border-slate-300 dark:border-slate-700 bg-white dark:bg-slate-900 p-4 shadow-xl",
                        onclick: move |evt| evt.stop_propagation(),
                        div { class: "mb-2 flex items-center justify-between",
                            span { class: "text-xs font-semibold uppercase tracking-wide text-slate-500 dark:text-slate-400",
                                "Cell value"
                            }
                            button {
                                class: "rounded px-2 py-0.5 text-sm text-slate-500 dark:text-slate-400 hover:bg-slate-200 dark:hover:bg-slate-800 hover:text-slate-900 dark:hover:text-slate-100",
                                aria_label: "Close",
                                onclick: move |_| expanded.set(None),
                                "✕"
                            }
                        }
                        match view {
                            // A short value already in hand is always the full
                            // value, so JSON pretty-printing is safe (FRE-77).
                            ExpandView::Text(value) => {
                                let display = pretty_json(&value).unwrap_or_else(|| value.clone());
                                rsx! {
                                    CopyRawButton { raw: value.clone() }
                                    pre { class: "whitespace-pre-wrap break-words font-mono text-xs text-slate-900 dark:text-slate-200",
                                        "{display}"
                                    }
                                }
                            },
                            ExpandView::Fetch { locator, column } => rsx! {
                                ExpandedValue { id, table: table.clone(), locator, column }
                            },
                        }
                    }
                }
            }
        }
    }
}

/// Which designed empty state a zero-row grid page shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GridEmpty {
    /// The table (matching the active filter, if any) has no rows.
    Table,
    /// A filter is active and nothing matches it — offers "Clear filter".
    NoMatch,
}

/// Selects the grid's empty state: `None` when the page has rows or pending
/// inserts to render, otherwise the no-filter-match state when a filter is
/// applied ([`GridEmpty::NoMatch`]) or the empty-table state
/// ([`GridEmpty::Table`]) when the whole table is empty.
fn empty_state(no_visible_rows: bool, has_filter: bool) -> Option<GridEmpty> {
    if !no_visible_rows {
        return None;
    }
    Some(if has_filter {
        GridEmpty::NoMatch
    } else {
        GridEmpty::Table
    })
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
    /// The fetched value. For a truncated large cell (`preview` is `Some`)
    /// this is only a bounded PREVIEW — never stage it as an edit; the full
    /// value is loaded on demand (FRE-33).
    value: Value,
    dirty: bool,
    /// Full-value metadata when this cell is a truncated preview; `None` for a
    /// complete value (and always `None` for a dirty/staged cell, whose value
    /// is the user's full staged input).
    preview: Option<PreviewInfo>,
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
    previews: &[Vec<Option<PreviewInfo>>],
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
    for (row_index, row) in result.rows.iter().enumerate() {
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
                // A staged cell holds the user's full value, so it is never a
                // preview; otherwise carry the fetched cell's preview metadata.
                let preview = if staged.is_some() {
                    None
                } else {
                    previews
                        .get(row_index)
                        .and_then(|cells| cells.get(index))
                        .copied()
                        .flatten()
                };
                CellView {
                    dirty: staged.is_some(),
                    editable: locator.is_some() && !deleted && !matches!(value, Value::Blob(_)),
                    value: staged.unwrap_or(value).clone(),
                    preview,
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

/// The display string for a cell, accounting for truncated previews: a
/// truncated text/json cell shows its preview with an ellipsis; a truncated
/// blob shows its real size (the preview only holds a prefix); everything
/// else displays normally.
fn cell_display(cell: &CellView) -> String {
    match (&cell.value, cell.preview) {
        (Value::Text(preview), Some(_)) => format!("{preview}…"),
        (_, Some(info)) if info.binary => format!("<blob {}>", human_bytes(info.full_len)),
        _ => cell.value.display(),
    }
}

/// Stable list key for one row: the row key when the row is addressable,
/// else its page position.
fn row_render_key(row: &RowView, index: usize) -> String {
    match &row.key {
        Some(key) => format!("r{key}"),
        None => format!("i{index}"),
    }
}

/// The staged-delete locators of a selection, in row-key order — a
/// deterministic order means identical selections always stage identical
/// change lists (stable failure indexes, stable confirm snapshots).
fn selection_locators(selected: &HashMap<String, RowLocator>) -> Vec<RowLocator> {
    let mut keys: Vec<&String> = selected.keys().collect();
    keys.sort();
    keys.into_iter().map(|key| selected[key].clone()).collect()
}

/// Save is blocked while a save is in flight or any pending insert still
/// lacks a required column.
fn save_disabled(saving: bool, missing_required: usize) -> bool {
    saving || missing_required > 0
}

/// The red inline message shown while required insert cells are unfilled.
fn required_missing_message(missing: usize) -> Option<String> {
    (missing > 0).then(|| format!("{missing} required column(s) missing in pending insert(s)"))
}

/// The Save button's label. Once the exact-count confirmation is armed it
/// spells out precisely what the second click commits.
fn save_button_label(saving: bool, armed: bool, deletes: usize, others: usize) -> String {
    if saving {
        return "Saving…".to_string();
    }
    if !armed {
        return "Save".to_string();
    }
    if others == 0 {
        format!("Confirm: delete {deletes}")
    } else {
        format!("Confirm: delete {deletes} + save {others}")
    }
}

/// The armed-confirmation notice, with the exact delete count.
fn confirm_notice(deletes: usize, others: usize) -> String {
    let rows = if deletes == 1 { "row" } else { "rows" };
    if others == 0 {
        format!("This will delete exactly {deletes} {rows}. Click again to save.")
    } else {
        format!(
            "This will delete exactly {deletes} {rows} and apply {others} other change(s). \
             Click again to save."
        )
    }
}

/// Per-column editor kind + nullability from introspected metadata.
/// Database-assigned `GENERATED ALWAYS` columns become a read-only kind
/// rather than inviting doomed input. Empty when the schema isn't ready.
fn column_kinds_of(meta: Option<&TableMeta>) -> HashMap<String, (EditorKind, bool)> {
    meta.map(|meta| {
        meta.columns
            .iter()
            .map(|c| {
                let kind = if c.generated == Generated::Always {
                    EditorKind::Generated
                } else {
                    editor_kind(&c.type_name)
                };
                (c.name.clone(), (kind, c.nullable))
            })
            .collect()
    })
    .unwrap_or_default()
}

/// Whether a cell may open an editor: the row allows it (locator present,
/// not deleted, value not a blob) and the column's type is editable
/// (blob and database-generated columns are read-only).
fn cell_editable(cell: &CellView, column_kinds: &HashMap<String, (EditorKind, bool)>) -> bool {
    cell.editable && !cell_kind(cell, column_kinds).0.is_read_only()
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

/// A move requested by a grid-navigation key (FRE-15), resolved by
/// [`apply_grid_move`] into a new focused cell or a page change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GridMove {
    Up,
    Down,
    Left,
    Right,
    RowStart,
    RowEnd,
    PageFirst,
    PageLast,
    PrevPage,
    NextPage,
}

/// Maps a physical key (plus whether Ctrl is held) to a grid move, or `None`
/// for keys the grid doesn't navigate on. Matches on `Code` (physical key),
/// layout- and IME-independent, consistent with the cell editor.
fn grid_move_for(code: Code, ctrl: bool) -> Option<GridMove> {
    Some(match code {
        Code::ArrowUp => GridMove::Up,
        Code::ArrowDown => GridMove::Down,
        Code::ArrowLeft => GridMove::Left,
        Code::ArrowRight => GridMove::Right,
        Code::Home if ctrl => GridMove::PageFirst,
        Code::Home => GridMove::RowStart,
        Code::End if ctrl => GridMove::PageLast,
        Code::End => GridMove::RowEnd,
        Code::PageUp => GridMove::PrevPage,
        Code::PageDown => GridMove::NextPage,
        _ => return None,
    })
}

/// The outcome of a grid move against a `rows`×`cols` page from `pos`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FocusOutcome {
    /// A new focused cell on the same page.
    Cell((usize, usize)),
    PrevPage,
    NextPage,
}

/// Resolves a [`GridMove`] from `pos` (row, col) within a `rows`×`cols` page.
/// Arrow/Home/End moves clamp at the page edges — they never cross pages;
/// PageUp/PageDown do that deliberately, so cell motion stays predictable.
/// `pos` is clamped into range first, so a stale focus (the page just shrank)
/// can't index out of bounds. Assumes `rows > 0` and `cols > 0`.
fn apply_grid_move(pos: (usize, usize), mv: GridMove, rows: usize, cols: usize) -> FocusOutcome {
    let r = pos.0.min(rows - 1);
    let c = pos.1.min(cols - 1);
    match mv {
        GridMove::Up => FocusOutcome::Cell((r.saturating_sub(1), c)),
        GridMove::Down => FocusOutcome::Cell(((r + 1).min(rows - 1), c)),
        GridMove::Left => FocusOutcome::Cell((r, c.saturating_sub(1))),
        GridMove::Right => FocusOutcome::Cell((r, (c + 1).min(cols - 1))),
        GridMove::RowStart => FocusOutcome::Cell((r, 0)),
        GridMove::RowEnd => FocusOutcome::Cell((r, cols - 1)),
        GridMove::PageFirst => FocusOutcome::Cell((0, 0)),
        GridMove::PageLast => FocusOutcome::Cell((rows - 1, cols - 1)),
        GridMove::PrevPage => FocusOutcome::PrevPage,
        GridMove::NextPage => FocusOutcome::NextPage,
    }
}

/// The half-open range of row indices to render for a scroll position
/// (FRE-32): windowed rendering keeps only these rows in the DOM. `first` is
/// the row at the top of the viewport (`scroll_top / row_height`); the window
/// spans the viewport's worth of rows plus `overscan` on each side, clamped to
/// `[0, total]`. A zero `total` or non-positive `row_height` yields an empty
/// `(0, 0)` range. `end >= start` always holds.
fn compute_visible_range(
    scroll_top: f64,
    viewport: f64,
    row_height: f64,
    total: usize,
    overscan: usize,
) -> (usize, usize) {
    if total == 0 || row_height <= 0.0 {
        return (0, 0);
    }
    let first = (scroll_top.max(0.0) / row_height).floor() as usize;
    let visible = (viewport.max(0.0) / row_height).ceil() as usize + 1;
    let end = first
        .saturating_add(visible)
        .saturating_add(overscan)
        .min(total);
    // Derive `start` backward from the clamped `end` (window = viewport rows +
    // overscan on both sides). In the middle of the page this is identical to
    // `first - overscan`, but when `end` clamps to `total` it keeps a full
    // window at the bottom — so a momentum fling that overshoots `scroll_top`
    // past the content can never leave an empty range (a blank viewport).
    let window = visible.saturating_add(2 * overscan);
    let start = end.saturating_sub(window);
    (start, end)
}

/// Keyboard-navigation snapshot of the visible page (FRE-15): enough per-cell
/// data (row key, column, editability, display text) for the focusable grid
/// container's key handler to move the focus ring and open the editor without
/// threading render-time borrows into the `'static` closure. Built by a memo
/// from the same fetched page + stage the grid renders.
#[derive(Debug, Default, Clone, PartialEq)]
struct GridNav {
    headers: Vec<String>,
    rows: Vec<GridNavRow>,
}

#[derive(Debug, Clone, PartialEq)]
struct GridNavRow {
    /// Row key ([`RowLocator::key`]) when addressable — needed to open the
    /// editor; `None` rows can only have their value expanded.
    key: Option<String>,
    /// The row's locator, used to fetch a truncated cell's full value on
    /// expand (FRE-33).
    locator: Option<RowLocator>,
    cells: Vec<GridNavCell>,
}

#[derive(Debug, Clone, PartialEq)]
struct GridNavCell {
    column: String,
    editable: bool,
    display: String,
    /// Whether this cell is a truncated preview — Enter expands it by fetching
    /// the full value rather than showing the in-hand preview (FRE-33).
    truncated: bool,
}

impl GridNav {
    fn build(
        headers: Vec<String>,
        rows: &[RowView],
        column_kinds: &HashMap<String, (EditorKind, bool)>,
    ) -> Self {
        let rows = rows
            .iter()
            .map(|row| GridNavRow {
                key: row.key.clone(),
                locator: row.locator.clone(),
                cells: row
                    .cells
                    .iter()
                    .map(|cell| GridNavCell {
                        column: cell.column.clone(),
                        editable: cell_editable(cell, column_kinds),
                        display: cell_display(cell),
                        truncated: cell.preview.is_some(),
                    })
                    .collect(),
            })
            .collect();
        GridNav { headers, rows }
    }

    /// (rows on the page, columns); a zero in either means nothing to focus.
    fn dims(&self) -> (usize, usize) {
        (self.rows.len(), self.headers.len())
    }
}

/// Opens a native save dialog and, on a chosen path, streams the current
/// grid view (active filter + sort, all matching rows — no paging) to it in
/// `format`. The export runs in the background via
/// [`AppState::export_query`]; the UI never blocks.
fn spawn_grid_export(
    state: AppState,
    id: ConnectionId,
    table: TableRef,
    dialect: Dialect,
    sort: Option<(String, SortDir)>,
    filter: Option<Filter>,
    format: ExportFormat,
) {
    let request = PageRequest {
        schema: table.schema.clone(),
        table: table.name.clone(),
        limit: 0,
        offset: 0,
        sort,
        filter,
        extra_key_column: None,
    };
    let (sql, params) = request.export_sql(dialect);
    let suggested = format!("{}.{}", table.name, format.extension());
    let (filter_name, ext) = match format {
        ExportFormat::Csv => ("CSV", "csv"),
        ExportFormat::Json => ("JSON", "json"),
    };
    spawn(async move {
        let picked = rfd::AsyncFileDialog::new()
            .set_title("Export table")
            .set_file_name(suggested)
            .add_filter(filter_name, &[ext])
            .save_file()
            .await;
        if let Some(file) = picked {
            state.export_query(id, sql, params, format, file.path().to_path_buf());
        }
    });
}

/// A short chip describing an equality (foreign-key) filter, e.g.
/// `artist_id = 1, seq = 2`. `None` for no filter or the single-column filter
/// bar filter (which the inputs already show).
fn equality_filter_label(filter: Option<&Filter>) -> Option<String> {
    match filter? {
        Filter::Equalities(pairs) => Some(
            pairs
                .iter()
                .map(|(column, value)| format!("{column} = {}", value.display()))
                .collect::<Vec<_>>()
                .join(", "),
        ),
        Filter::Column { .. } => None,
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
        applied_filter.set(Some(Filter::Column {
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
        th { class: "border-b border-slate-300 dark:border-slate-700 px-3 py-1.5",
            button {
                class: "font-mono text-xs font-semibold text-slate-900 dark:text-slate-300 hover:text-slate-950 dark:hover:text-white",
                onclick: move |_| on_sort.call(clicked_name.clone()),
                "{name}{marker}"
            }
        }
    }
}

/// One fetched row: staged tint/strike-through, an optional leading
/// selection checkbox (editable tables), and one [`GridCellSlot`] per cell
/// (which renders either the display cell or, for the active cell, the
/// in-place editor).
#[component]
fn GridRow(
    id: ConnectionId,
    table: TableRef,
    row: RowView,
    column_kinds: HashMap<String, (EditorKind, bool)>,
    /// Foreign keys of this table, indexed by `col_to_fk` (FRE-29).
    foreign_keys: Vec<ForeignKeyMeta>,
    /// Referencing column → index into `foreign_keys` (first FK wins).
    col_to_fk: HashMap<String, usize>,
    dialect: Dialect,
    editing: Signal<Option<ActiveEdit>>,
    /// The keyboard-focused column in this row (FRE-15), or `None` when the
    /// focus ring is on another row.
    focused_col: Option<usize>,
    select_enabled: bool,
    mut selected: Signal<HashMap<String, RowLocator>>,
    /// Follows the FK a clicked cell belongs to, carrying that FK plus this
    /// row's column → value map (the source of the jump's equality filter).
    on_fk_jump: EventHandler<(ForeignKeyMeta, HashMap<String, Value>)>,
) -> Element {
    // This row's Tab order: its editable columns, left to right.
    let editable_columns: Vec<String> = row
        .cells
        .iter()
        .filter(|cell| cell_editable(cell, &column_kinds))
        .map(|cell| cell.column.clone())
        .collect();
    // The row's values by column, the source for any FK jump from this row.
    let row_values: HashMap<String, Value> = row
        .cells
        .iter()
        .map(|cell| (cell.column.clone(), cell.value.clone()))
        .collect();
    // Rows pending delete (or unaddressable) can't be (re)selected; their
    // leading cell stays empty.
    let checkbox: Option<(String, RowLocator)> = match (&row.key, &row.locator) {
        (Some(key), Some(locator)) if !row.deleted => Some((key.clone(), locator.clone())),
        _ => None,
    };
    rsx! {
        tr {
            class: if row.deleted {
                // Pending delete: red tint + strike-through.
                "border-t border-slate-200 dark:border-slate-800/60 bg-red-100 dark:bg-red-950/40 line-through decoration-red-400/60"
            } else {
                "border-t border-slate-200 dark:border-slate-800/60 hover:bg-slate-100 dark:hover:bg-slate-800/30"
            },
            // Uniform row height (FRE-32): the windowed renderer positions rows
            // by `ROW_HEIGHT`, so pin it here — this also stops an open inline
            // editor from making its row taller and drifting the offsets.
            style: "height:{ROW_HEIGHT}px;",
            if select_enabled {
                td { class: "w-8 px-2 py-1",
                    if let Some((key, locator)) = checkbox {
                        input {
                            r#type: "checkbox",
                            class: "accent-red-500",
                            checked: selected.read().contains_key(&key),
                            oninput: move |_| {
                                let mut map = selected.peek().clone();
                                if map.remove(&key).is_none() {
                                    map.insert(key.clone(), locator.clone());
                                }
                                selected.set(map);
                            },
                        }
                    }
                }
            }
            for (col_index , cell) in row.cells.clone().into_iter().enumerate() {
                GridCellSlot {
                    key: "{cell.column}",
                    id,
                    table: table.clone(),
                    row_key: row.key.clone(),
                    locator: row.locator.clone(),
                    kind: cell_kind(&cell, &column_kinds).0,
                    nullable: cell_kind(&cell, &column_kinds).1,
                    editable: cell_editable(&cell, &column_kinds),
                    focused: focused_col == Some(col_index),
                    cell: cell.clone(),
                    dialect,
                    editable_columns: editable_columns.clone(),
                    editing,
                    // FK cells (non-NULL value belonging to an FK) carry the
                    // jump payload: the FK plus this row's values. A NULL FK
                    // references nothing, so it renders as a plain cell.
                    fk_jump: col_to_fk
                        .get(&cell.column)
                        .filter(|_| !cell.value.is_null())
                        .map(|&index| (foreign_keys[index].clone(), row_values.clone())),
                    on_fk_jump,
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
    /// Whether this cell holds the grid's keyboard focus ring (FRE-15).
    focused: bool,
    dialect: Dialect,
    editable_columns: Vec<String>,
    mut editing: Signal<Option<ActiveEdit>>,
    /// `Some((fk, row_values))` when this cell belongs to a foreign key and
    /// has a non-NULL value — renders a ↗ jump link (FRE-29). Editing the
    /// cell value stays on double-click/Enter, so navigation and editing never
    /// contend for the same gesture.
    fk_jump: Option<(ForeignKeyMeta, HashMap<String, Value>)>,
    on_fk_jump: EventHandler<(ForeignKeyMeta, HashMap<String, Value>)>,
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
        // A truncated cell holds only a preview — the editor must load the
        // full current value first, so a preview can never be staged as the
        // new value and silently truncate the stored data (FRE-33).
        if cell.preview.is_some() {
            return rsx! {
                TruncatedCellEditor {
                    id,
                    table: table.clone(),
                    locator,
                    row_key,
                    column,
                    kind,
                    dialect,
                    nullable,
                    editable_columns,
                    editing,
                }
            };
        }
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
        // Blob and generated cells explain why they're locked; other
        // read-only cells (views, keyless tables) are covered by the
        // grid-level notice.
        let display = cell_display(&cell);
        let tooltip = if cell.preview.is_some() {
            "Truncated preview — press Enter to view (or edit) the full value".to_string()
        } else if kind == EditorKind::Generated {
            "generated by the database — read-only".to_string()
        } else if kind == EditorKind::Blob || matches!(cell.value, Value::Blob(_)) {
            "blobs are read-only".to_string()
        } else {
            display.clone()
        };
        let text = match &cell.value {
            Value::Null => "font-mono text-xs italic text-slate-400 dark:text-slate-600",
            Value::Blob(_) => "font-mono text-xs text-violet-700 dark:text-violet-400",
            _ => "font-mono text-xs text-slate-900 dark:text-slate-200",
        };
        // The keyboard focus ring (FRE-15). Theme-aware; inset so it reads
        // over the cell borders. The focused cell carries `dv-focused-cell`
        // so the grid can scroll it into view.
        let ring = if focused {
            " ring-2 ring-inset ring-sky-500 dark:ring-sky-400"
        } else {
            ""
        };
        let class = if cell.dirty {
            format!("px-3 py-1 {text} bg-amber-100 dark:bg-amber-900/40{ring}")
        } else {
            format!("px-3 py-1 {text}{ring}")
        };
        rsx! {
            td {
                class,
                id: if focused { "dv-focused-cell" },
                // Double-click opens the editor with the mouse; keyboard
                // activation (Enter) is handled centrally by the grid
                // container via the focus ring (FRE-15).
                ondoubleclick: move |_| {
                    if editable {
                        activate();
                    }
                },
                div { class: "flex items-center gap-1",
                    div { class: "max-w-md truncate", title: "{tooltip}", "{display}" }
                    // FK jump affordance: a single click follows the key to the
                    // referenced row. Kept distinct from the cell body so the
                    // double-click / Enter edit gesture is untouched.
                    if let Some((fk, row_values)) = fk_jump.clone() {
                        a {
                            class: "shrink-0 cursor-pointer select-none text-cyan-600 dark:text-cyan-400 hover:underline",
                            title: "Go to {fk.referenced_table}",
                            onclick: move |evt| {
                                evt.stop_propagation();
                                on_fk_jump.call((fk.clone(), row_values.clone()));
                            },
                            "↗"
                        }
                    }
                }
            }
        }
    }
}

/// One pending-insert phantom row (green tint, dashed edge): a leading ✕
/// cell that removes the phantom (staging nothing — see
/// [`AppState::remove_pending_insert`]), then one [`InsertCellSlot`] per
/// visible column, sharing the grid's editing state and interaction model.
#[component]
fn InsertRow(
    id: ConnectionId,
    table: TableRef,
    insert: PendingInsert,
    headers: Vec<String>,
    column_kinds: HashMap<String, (EditorKind, bool)>,
    required: HashSet<String>,
    dialect: Dialect,
    /// Whether the grid renders the leading checkbox column (it does
    /// whenever inserts are possible; this keeps the phantom row aligned).
    lead_cell: bool,
    editing: Signal<Option<ActiveEdit>>,
) -> Element {
    let state = use_context::<AppState>();
    let insert_id = insert.id();
    let row_key = insert.row_key();
    // Tab order: every editable column (blob and generated cells stay
    // "default" — there is no blob editor, and generated columns are
    // database-assigned). Columns missing from the metadata edit as text,
    // same fallback as existing rows.
    let editable_columns: Vec<String> = headers
        .iter()
        .filter(|header| {
            column_kinds
                .get(*header)
                .map(|(kind, _)| !kind.is_read_only())
                .unwrap_or(true)
        })
        .cloned()
        .collect();
    let remove_table = table.clone();
    rsx! {
        tr { class: "border-t border-dashed border-emerald-300 dark:border-emerald-700/60 bg-emerald-100 dark:bg-emerald-950/40",
            if lead_cell {
                td { class: "w-8 px-2 py-1",
                    button {
                        class: "rounded px-1.5 text-xs text-emerald-700 dark:text-emerald-300/80 hover:bg-red-100 dark:hover:bg-red-900/40 hover:text-red-600 dark:hover:text-red-300",
                        title: "Remove this pending insert (stages nothing)",
                        onclick: move |_| state.remove_pending_insert(id, &remove_table, insert_id),
                        "✕"
                    }
                }
            }
            for column in headers.clone() {
                InsertCellSlot {
                    key: "{column}",
                    id,
                    table: table.clone(),
                    insert_id,
                    row_key: row_key.clone(),
                    column: column.clone(),
                    override_value: insert.value(&column).cloned(),
                    kind: column_kinds.get(&column).map(|(kind, _)| *kind).unwrap_or(EditorKind::Text),
                    nullable: column_kinds.get(&column).map(|(_, nullable)| *nullable).unwrap_or(true),
                    missing: required.contains(&column) && insert.lacks_value(&column),
                    dialect,
                    editable_columns: editable_columns.clone(),
                    editing,
                }
            }
        }
    }
}

/// One cell of a phantom insert row. Displays dim italic "default" until
/// overridden (the column is then omitted from the INSERT — serial/identity
/// and defaulted columns get their database value); an overridden cell
/// shows the concrete staged value on the dirty tint. Unfilled REQUIRED
/// cells carry a red ring. Opens the shared [`CellEditor`] on
/// double-click/Enter (same model as existing rows) with the extra ↺
/// revert-to-default action; commits stage per-column overrides via
/// [`AppState::stage_insert_value`].
///
/// Blob-typed columns are not editable (no blob editor yet) and always stay
/// "default" — a required blob column can therefore never be filled here;
/// the phantom row must be removed instead.
#[component]
fn InsertCellSlot(
    id: ConnectionId,
    table: TableRef,
    insert_id: u64,
    row_key: String,
    column: String,
    override_value: Option<Value>,
    kind: EditorKind,
    nullable: bool,
    missing: bool,
    dialect: Dialect,
    editable_columns: Vec<String>,
    mut editing: Signal<Option<ActiveEdit>>,
) -> Element {
    let state = use_context::<AppState>();
    let editable = !kind.is_read_only();
    let is_active = editable
        && editing
            .read()
            .as_ref()
            .is_some_and(|active| active.row_key == row_key && active.column == column);

    if is_active {
        let commit_table = table.clone();
        let commit_column = column.clone();
        let commit_row_key = row_key.clone();
        let default_table = table.clone();
        let default_column = column.clone();
        rsx! {
            CellEditor {
                kind,
                dialect,
                nullable,
                initial: override_value.clone().unwrap_or(Value::Null),
                on_commit: move |(value, nav): (Option<Value>, EditNav)| {
                    if let Some(value) = value {
                        state.stage_insert_value(id, &commit_table, insert_id, &commit_column, value);
                    }
                    let next = match nav {
                        EditNav::Stay => None,
                        EditNav::Next => step_column(&editable_columns, &commit_column, 1),
                        EditNav::Prev => step_column(&editable_columns, &commit_column, -1),
                    };
                    editing.set(next.map(|column| ActiveEdit {
                        row_key: commit_row_key.clone(),
                        column,
                    }));
                },
                on_cancel: move |_| editing.set(None),
                on_default: move |_| {
                    state.clear_insert_value(id, &default_table, insert_id, &default_column);
                    editing.set(None);
                },
            }
        }
    } else {
        let activate_key = row_key.clone();
        let activate_column = column.clone();
        let mut activate = move || {
            editing.set(Some(ActiveEdit {
                row_key: activate_key.clone(),
                column: activate_column.clone(),
            }));
        };
        let mut open_on_enter = activate.clone();
        let (display, text_class) = match &override_value {
            // Not overridden: the database decides (default / serial /
            // identity / NULL).
            None => (
                "default".to_string(),
                "font-mono text-xs italic text-emerald-600 dark:text-emerald-500/70",
            ),
            Some(Value::Null) => (
                Value::Null.display(),
                "font-mono text-xs italic text-slate-400 dark:text-slate-600",
            ),
            Some(value) => (
                value.display(),
                "font-mono text-xs text-slate-900 dark:text-slate-200",
            ),
        };
        let tooltip = if missing {
            "required: NOT NULL without a default — fill in before saving".to_string()
        } else if override_value.is_none() {
            "left to the database default".to_string()
        } else {
            display.clone()
        };
        let dirty_tint = if override_value.is_some() {
            " bg-amber-100 dark:bg-amber-900/40"
        } else {
            ""
        };
        let missing_ring = if missing {
            " ring-1 ring-inset ring-red-500"
        } else {
            ""
        };
        let class = format!("px-3 py-1 {text_class}{dirty_tint}{missing_ring}");
        rsx! {
            td {
                class,
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

/// The in-place editor for a TRUNCATED cell (FRE-33): loads the full current
/// value via [`AppState::load_cell`] first, then hands it to the shared
/// [`CellEditor`] so the edit starts from the complete value — a preview is
/// NEVER staged as the new value (which would silently truncate the stored
/// data). A value larger than the fetch cap can't be edited inline at all
/// (the prefix would still corrupt it); it shows a read-only note instead.
#[component]
fn TruncatedCellEditor(
    id: ConnectionId,
    table: TableRef,
    locator: RowLocator,
    row_key: String,
    column: String,
    kind: EditorKind,
    dialect: Dialect,
    nullable: bool,
    editable_columns: Vec<String>,
    mut editing: Signal<Option<ActiveEdit>>,
) -> Element {
    let state = use_context::<AppState>();
    let fetch_table = table.clone();
    let fetch_locator = locator.clone();
    let fetch_column = column.clone();
    let cell = use_resource(move || {
        let table = fetch_table.clone();
        let locator = fetch_locator.clone();
        let column = fetch_column.clone();
        async move { state.load_cell(id, table, locator, column).await }
    });
    let loaded = cell.read();
    match loaded.as_ref() {
        None => rsx! {
            td { class: "px-2 py-1", DelayedLoading { label: "Loading full value…" } }
        },
        Some(Err(err)) => {
            let err = err.clone();
            rsx! {
                td { class: "px-2 py-1",
                    div { class: "flex items-center gap-2",
                        Banner { kind: BannerKind::Error, message: err }
                        button {
                            class: "shrink-0 rounded border border-slate-400 dark:border-slate-600 px-2 py-0.5 text-xs",
                            onclick: move |_| editing.set(None),
                            "Close"
                        }
                    }
                }
            }
        }
        Some(Ok(fetch)) if fetch.capped => {
            let note = format!(
                "This value is too large to edit inline (over {}). Open it with Expand to view it.",
                human_bytes(FETCH_CELL_MAX_BYTES as u64),
            );
            rsx! {
                td { class: "px-2 py-1",
                    div { class: "flex items-center gap-2",
                        Banner { kind: BannerKind::Warning, message: note }
                        button {
                            class: "shrink-0 rounded border border-slate-400 dark:border-slate-600 px-2 py-0.5 text-xs",
                            onclick: move |_| editing.set(None),
                            "Close"
                        }
                    }
                }
            }
        }
        Some(Ok(fetch)) => {
            let initial = fetch.value.clone();
            let commit_table = table.clone();
            let commit_column = column.clone();
            let commit_row_key = row_key.clone();
            let commit_columns = editable_columns.clone();
            let commit_locator = locator.clone();
            rsx! {
                CellEditor {
                    kind,
                    dialect,
                    nullable,
                    initial,
                    on_commit: move |(value, nav): (Option<Value>, EditNav)| {
                        if let Some(value) = value {
                            state.stage_cell_edit(id, &commit_table, commit_locator.clone(), &commit_column, value);
                        }
                        let next = match nav {
                            EditNav::Stay => None,
                            EditNav::Next => step_column(&commit_columns, &commit_column, 1),
                            EditNav::Prev => step_column(&commit_columns, &commit_column, -1),
                        };
                        editing.set(next.map(|column| ActiveEdit {
                            row_key: commit_row_key.clone(),
                            column,
                        }));
                    },
                    on_cancel: move |_| editing.set(None),
                }
            }
        }
    }
}

/// The full-value body of the expand popup for a truncated cell (FRE-33):
/// loads the value via [`AppState::load_cell`] and renders it. Text/json show
/// in full (a value over the fetch cap shows its first chunk with a note);
/// binary values show their size only.
#[component]
fn ExpandedValue(
    id: ConnectionId,
    table: TableRef,
    locator: RowLocator,
    column: String,
) -> Element {
    let state = use_context::<AppState>();
    let fetch_table = table.clone();
    let fetch_locator = locator.clone();
    let fetch_column = column.clone();
    let cell = use_resource(move || {
        let table = fetch_table.clone();
        let locator = fetch_locator.clone();
        let column = fetch_column.clone();
        async move { state.load_cell(id, table, locator, column).await }
    });
    let loaded = cell.read();
    match loaded.as_ref() {
        None => rsx! {
            DelayedLoading { label: "Loading full value…" }
        },
        Some(Err(err)) => rsx! {
            Banner { kind: BannerKind::Error, message: err.clone() }
        },
        Some(Ok(fetch)) => {
            if let Value::Blob(_) = &fetch.value {
                return rsx! {
                    p { class: "font-mono text-xs text-slate-500 dark:text-slate-400",
                        "Binary value — {human_bytes(fetch.full_len)} (content not shown)."
                    }
                };
            }
            let text = fetch.value.display();
            let capped = fetch.capped;
            // A capped fetch may be cut mid-document — only pretty-print
            // JSON when the full value is held (FRE-77).
            let display = if capped {
                text.clone()
            } else {
                pretty_json(&text).unwrap_or_else(|| text.clone())
            };
            rsx! {
                if capped {
                    p { class: "mb-2 rounded border border-amber-300 dark:border-amber-800/70 bg-amber-50 dark:bg-amber-950/40 px-2 py-1 text-xs text-amber-700 dark:text-amber-300",
                        "Value is very large; showing the first {human_bytes(FETCH_CELL_MAX_BYTES as u64)}."
                    }
                }
                CopyRawButton { raw: text.clone() }
                pre { class: "whitespace-pre-wrap break-words font-mono text-xs text-slate-900 dark:text-slate-200",
                    "{display}"
                }
            }
        }
    }
}

/// Pretty-printed rendering of a JSON cell value for the expand popup
/// (FRE-77): `Some` only when the text parses as a JSON object or array —
/// scalars re-serialize identically and everything else (including a JSON
/// document cut off by the fetch cap) falls back to the raw text.
fn pretty_json(text: &str) -> Option<String> {
    let parsed: serde_json::Value = serde_json::from_str(text).ok()?;
    if !matches!(
        parsed,
        serde_json::Value::Object(_) | serde_json::Value::Array(_)
    ) {
        return None;
    }
    serde_json::to_string_pretty(&parsed).ok()
}

/// A right-aligned "Copy raw" action for the expand popup (FRE-77): always
/// copies the raw cell value — the pane may be showing the pretty-printed
/// form, which is for reading, not round-tripping.
#[component]
fn CopyRawButton(raw: String) -> Element {
    let raw_json = serde_json::to_string(&raw).unwrap_or_else(|_| "\"\"".into());
    rsx! {
        div { class: "mb-1 flex justify-end",
            button {
                class: "rounded border border-slate-300 dark:border-slate-700 px-1.5 py-0.5 text-xs text-slate-500 dark:text-slate-400 hover:bg-slate-200 dark:hover:bg-slate-800 hover:text-slate-900 dark:hover:text-slate-100",
                title: "Copy the raw value (not the formatted view)",
                onclick: move |_| {
                    document::eval(&format!("navigator.clipboard.writeText({raw_json});"));
                },
                "Copy raw"
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pretty_json_formats_objects_and_arrays_only() {
        // Note: serde_json's default map sorts keys — fine for reading, and
        // jsonb has no stable key order anyway; Copy raw keeps the original.
        assert_eq!(
            pretty_json(r#"{"b":1,"a":[2,3]}"#).as_deref(),
            Some("{\n  \"a\": [\n    2,\n    3\n  ],\n  \"b\": 1\n}")
        );
        assert_eq!(pretty_json("[1,2]").as_deref(), Some("[\n  1,\n  2\n]"));
        // Scalars re-serialize identically — no point reformatting.
        assert_eq!(pretty_json("42"), None);
        assert_eq!(pretty_json("\"hi\""), None);
        // Non-JSON and truncated documents fall back to raw.
        assert_eq!(pretty_json("plain text"), None);
        assert_eq!(pretty_json(r#"{"cut": "mid-docu"#), None);
    }

    #[test]
    fn empty_state_selector_distinguishes_the_four_cases() {
        // Rows present (or pending inserts): no empty state at all.
        assert_eq!(empty_state(false, false), None);
        assert_eq!(empty_state(false, true), None);
        // Zero rows, no filter: the empty-table state.
        assert_eq!(empty_state(true, false), Some(GridEmpty::Table));
        // Zero rows with an active filter: the no-match state (distinct).
        assert_eq!(empty_state(true, true), Some(GridEmpty::NoMatch));
    }

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
        let rows = view_rows(&result, &[], 0, Some(&pk_identity()), Some(&stage));
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
        let rows = view_rows(&result, &[], 1, Some(&identity), Some(&stage));
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
        let rows = view_rows(&result, &[], 0, None, Some(&stage));
        assert!(rows.iter().all(|r| !r.deleted));
        assert!(rows.iter().flat_map(|r| r.cells.iter()).all(|c| !c.dirty));
    }

    #[test]
    fn editability_needs_a_locator_and_excludes_deletes_and_blobs() {
        let kinds: HashMap<String, (EditorKind, bool)> = [
            (
                "id".to_string(),
                (
                    EditorKind::Numeric {
                        kind: super::super::editing::NumericKind::Integer,
                    },
                    false,
                ),
            ),
            ("title".to_string(), (EditorKind::Text, true)),
            ("cover".to_string(), (EditorKind::Blob, true)),
        ]
        .into_iter()
        .collect();

        // With an identity, plain cells are editable…
        let result = two_column_result();
        let rows = view_rows(&result, &[], 0, Some(&pk_identity()), None);
        assert!(rows[0].locator.is_some());
        assert!(cell_editable(&rows[0].cells[1], &kinds));
        // …a column missing from the metadata falls back to editable text…
        let (kind, nullable) = cell_kind(&rows[0].cells[1], &HashMap::new());
        assert_eq!(kind, EditorKind::Text);
        assert!(nullable);
        // …but without an identity nothing is.
        let rows = view_rows(&result, &[], 0, None, None);
        assert!(rows[0].locator.is_none());
        assert!(!rows[0].cells[1].editable);

        // Rows pending deletion are not editable.
        let mut stage = TableStage::default();
        stage.mark_delete(RowLocator {
            identity_values: vec![Value::Integer(1)],
        });
        let rows = view_rows(&result, &[], 0, Some(&pk_identity()), Some(&stage));
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
        let rows = view_rows(&blob_result, &[], 0, Some(&pk_identity()), None);
        assert!(!rows[0].cells[1].editable, "blob value cell");
        let null_blob = QueryResult {
            rows: vec![vec![Value::Integer(1), Value::Null]],
            ..blob_result
        };
        let rows = view_rows(&null_blob, &[], 0, Some(&pk_identity()), None);
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
    fn selection_maps_to_staged_deletes_in_row_key_order() {
        let locator = |id: i64| RowLocator {
            identity_values: vec![Value::Integer(id)],
        };
        let selected: HashMap<String, RowLocator> = [
            (locator(2).key(), locator(2)),
            (locator(1).key(), locator(1)),
        ]
        .into_iter()
        .collect();

        let locators = selection_locators(&selected);
        assert_eq!(locators, vec![locator(1), locator(2)], "row-key order");

        // Staging the mapped locators marks exactly the selected rows.
        let mut stage = TableStage::default();
        for l in locators {
            stage.mark_delete(l);
        }
        assert!(stage.is_deleted(&locator(1).key()));
        assert!(stage.is_deleted(&locator(2).key()));
        assert!(!stage.is_deleted(&locator(3).key()));
        assert_eq!(stage.delete_count(), 2);
    }

    #[test]
    fn save_is_blocked_while_required_cells_are_missing_or_a_save_runs() {
        assert!(!save_disabled(false, 0));
        assert!(save_disabled(true, 0), "in-flight save");
        assert!(save_disabled(false, 2), "missing required cells");
        assert_eq!(required_missing_message(0), None);
        assert_eq!(
            required_missing_message(2),
            Some("2 required column(s) missing in pending insert(s)".into())
        );
    }

    #[test]
    fn grid_keys_map_to_moves() {
        // Arrows and edges.
        assert_eq!(grid_move_for(Code::ArrowUp, false), Some(GridMove::Up));
        assert_eq!(grid_move_for(Code::ArrowDown, false), Some(GridMove::Down));
        assert_eq!(grid_move_for(Code::ArrowLeft, false), Some(GridMove::Left));
        assert_eq!(
            grid_move_for(Code::ArrowRight, false),
            Some(GridMove::Right)
        );
        // Home/End switch to page-wide moves with Ctrl.
        assert_eq!(grid_move_for(Code::Home, false), Some(GridMove::RowStart));
        assert_eq!(grid_move_for(Code::Home, true), Some(GridMove::PageFirst));
        assert_eq!(grid_move_for(Code::End, false), Some(GridMove::RowEnd));
        assert_eq!(grid_move_for(Code::End, true), Some(GridMove::PageLast));
        // Paging keys.
        assert_eq!(grid_move_for(Code::PageUp, false), Some(GridMove::PrevPage));
        assert_eq!(
            grid_move_for(Code::PageDown, false),
            Some(GridMove::NextPage)
        );
        // Enter/Escape are handled separately, not as moves.
        assert_eq!(grid_move_for(Code::Enter, false), None);
        assert_eq!(grid_move_for(Code::KeyA, false), None);
    }

    #[test]
    fn arrow_moves_clamp_at_the_page_edges() {
        // 3 rows × 2 cols; arrows never leave the page.
        let up = |pos| apply_grid_move(pos, GridMove::Up, 3, 2);
        let down = |pos| apply_grid_move(pos, GridMove::Down, 3, 2);
        let left = |pos| apply_grid_move(pos, GridMove::Left, 3, 2);
        let right = |pos| apply_grid_move(pos, GridMove::Right, 3, 2);
        assert_eq!(down((0, 0)), FocusOutcome::Cell((1, 0)));
        assert_eq!(
            down((2, 0)),
            FocusOutcome::Cell((2, 0)),
            "clamp at last row"
        );
        assert_eq!(up((0, 1)), FocusOutcome::Cell((0, 1)), "clamp at first row");
        assert_eq!(up((2, 1)), FocusOutcome::Cell((1, 1)));
        assert_eq!(right((0, 0)), FocusOutcome::Cell((0, 1)));
        assert_eq!(
            right((0, 1)),
            FocusOutcome::Cell((0, 1)),
            "clamp at last col"
        );
        assert_eq!(
            left((0, 0)),
            FocusOutcome::Cell((0, 0)),
            "clamp at first col"
        );
        assert_eq!(left((1, 1)), FocusOutcome::Cell((1, 0)));
    }

    #[test]
    fn home_end_and_paging_moves() {
        assert_eq!(
            apply_grid_move((1, 1), GridMove::RowStart, 3, 4),
            FocusOutcome::Cell((1, 0))
        );
        assert_eq!(
            apply_grid_move((1, 1), GridMove::RowEnd, 3, 4),
            FocusOutcome::Cell((1, 3))
        );
        assert_eq!(
            apply_grid_move((2, 2), GridMove::PageFirst, 3, 4),
            FocusOutcome::Cell((0, 0))
        );
        assert_eq!(
            apply_grid_move((0, 0), GridMove::PageLast, 3, 4),
            FocusOutcome::Cell((2, 3))
        );
        assert_eq!(
            apply_grid_move((0, 0), GridMove::PrevPage, 3, 4),
            FocusOutcome::PrevPage
        );
        assert_eq!(
            apply_grid_move((0, 0), GridMove::NextPage, 3, 4),
            FocusOutcome::NextPage
        );
    }

    #[test]
    fn stale_focus_is_clamped_before_moving() {
        // Focus at (5, 5) but the page is only 2×2 (it just shrank): the move
        // resolves from the clamped (1, 1), never indexing out of bounds.
        assert_eq!(
            apply_grid_move((5, 5), GridMove::Up, 2, 2),
            FocusOutcome::Cell((0, 1))
        );
        assert_eq!(
            apply_grid_move((5, 5), GridMove::Left, 2, 2),
            FocusOutcome::Cell((1, 0))
        );
    }

    #[test]
    fn grid_nav_reports_dims_and_cell_editability() {
        let kinds: HashMap<String, (EditorKind, bool)> = [
            ("id".to_string(), (EditorKind::Text, false)),
            ("title".to_string(), (EditorKind::Text, true)),
        ]
        .into_iter()
        .collect();
        let result = two_column_result();
        let rows = view_rows(&result, &[], 0, Some(&pk_identity()), None);
        let nav = GridNav::build(vec!["id".into(), "title".into()], &rows, &kinds);
        assert_eq!(nav.dims(), (2, 2));
        // Both cells of an identified table are editable text here.
        assert!(nav.rows[0].cells[1].editable);
        assert_eq!(nav.rows[0].cells[1].column, "title");
        assert_eq!(nav.rows[0].cells[0].display, "1");
        // Without an identity, nothing is editable and rows have no key.
        let rows = view_rows(&result, &[], 0, None, None);
        let nav = GridNav::build(vec!["id".into(), "title".into()], &rows, &kinds);
        assert!(nav.rows[0].key.is_none());
        assert!(nav.rows.iter().all(|r| r.cells.iter().all(|c| !c.editable)));
    }

    #[test]
    fn visible_range_windows_around_the_scroll_position() {
        // 33px rows, a 330px viewport (~10 rows), overscan 8, 100 rows.
        // At the top: start clamps to 0, end covers ~10 visible + 1 + overscan.
        assert_eq!(compute_visible_range(0.0, 330.0, 33.0, 100, 8), (0, 19));
        // Scrolled to row 50 (50 * 33 = 1650): first = 50, window
        // 50-8 .. 50+11+8 = 42 .. 69.
        assert_eq!(compute_visible_range(1650.0, 330.0, 33.0, 100, 8), (42, 69));
        // Near the bottom the end clamps to `total` and start is derived
        // backward to keep a full window (first = floor(3200/33) = 96,
        // end = 100, window = 11 + 16 = 27, start = 100 - 27 = 73).
        assert_eq!(
            compute_visible_range(3200.0, 330.0, 33.0, 100, 8),
            (73, 100)
        );
    }

    #[test]
    fn visible_range_clamps_and_handles_empty() {
        // Empty page: nothing to render.
        assert_eq!(compute_visible_range(0.0, 600.0, 33.0, 0, 8), (0, 0));
        // Non-positive row height can't be divided by: empty range, no panic.
        assert_eq!(compute_visible_range(100.0, 600.0, 0.0, 50, 8), (0, 0));
        // A page shorter than the viewport renders in full.
        assert_eq!(compute_visible_range(0.0, 600.0, 33.0, 5, 8), (0, 5));
        // A scroll offset past the content still yields a valid clamped range
        // (end at total, start no greater than end).
        let (start, end) = compute_visible_range(99_999.0, 600.0, 33.0, 40, 8);
        assert_eq!(end, 40);
        assert!(start <= end);
    }

    #[test]
    fn save_button_spells_out_the_armed_delete_count() {
        assert_eq!(save_button_label(false, false, 3, 1), "Save");
        assert_eq!(save_button_label(true, true, 3, 1), "Saving…");
        assert_eq!(save_button_label(false, true, 3, 0), "Confirm: delete 3");
        assert_eq!(
            save_button_label(false, true, 2, 4),
            "Confirm: delete 2 + save 4"
        );
        assert_eq!(
            confirm_notice(1, 0),
            "This will delete exactly 1 row. Click again to save."
        );
        assert_eq!(
            confirm_notice(2, 3),
            "This will delete exactly 2 rows and apply 3 other change(s). Click again to save."
        );
    }
}
