use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use dioxus::prelude::*;
use dioxus_icons::lucide::{
    ChevronDown, ChevronUp, File, PanelRight, Pencil, RefreshCw, SearchX, ShieldAlert, X,
};

use crate::db::{
    raw_cell_text, render_copy, ColumnMeta, ConnectionId, CopyBlock, CopyFormat, Dialect,
    ExportFormat, Filter, FilterOp, ForeignKeyMeta, Generated, Page, PageRequest, PreviewInfo,
    QueryResult, RowIdentity, RowLocator, SortDir, StagedChange, TableAccess, TableMeta, Value,
    FETCH_CELL_MAX_BYTES,
};
use crate::util::human_bytes;

use super::editing::{editor_kind, CellEditor, EditNav, EditorKind};
use super::notice::{Banner, BannerKind, DelayedLoading, EmptyState};
use super::schema::display_type;
use super::selection::Selection;
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

/// Render data shared by many components, cloned by refcount rather than
/// deep-copied into every one of them (FRE-130).
///
/// The grid hands the same per-table metadata to ~30 windowed rows (and the
/// same row values to every cell of a row) on every render. As plain
/// `HashMap`/`Vec` props that is a deep copy per row per render, plus a deep
/// comparison per row when Dioxus decides whether the row changed. Behind this
/// wrapper the clone is an `Arc` bump and the comparison is a pointer check.
///
/// The pointer check is a fast path, not the whole answer: the memos that
/// build these values re-run whenever *any* key of a whole-map signal changes,
/// so a rebuild that produced an identical value must still gate its
/// dependents. Falling back to the structural comparison keeps that gating —
/// the deep compare then costs once per rebuild instead of once per row per
/// render. (Contrast [`SharedStatement`](super::state::SharedStatement), which
/// is pointer-eq only: its payload is an immutable query result that is never
/// rebuilt from equal inputs.)
struct Shared<T>(Arc<T>);

impl<T> Shared<T> {
    fn new(value: T) -> Self {
        Shared(Arc::new(value))
    }
}

// Hand-written rather than derived: `#[derive(Clone)]` on a generic struct
// would demand `T: Clone`, which is exactly what this wrapper exists to avoid.
impl<T> Clone for Shared<T> {
    fn clone(&self) -> Self {
        Shared(Arc::clone(&self.0))
    }
}

impl<T: std::fmt::Debug> std::fmt::Debug for Shared<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

impl<T> std::ops::Deref for Shared<T> {
    type Target = T;

    fn deref(&self) -> &T {
        &self.0
    }
}

impl<T: PartialEq> PartialEq for Shared<T> {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0) || self.0 == other.0
    }
}

impl<T: Default> Default for Shared<T> {
    fn default() -> Self {
        Shared::new(T::default())
    }
}

/// The cell whose in-place editor is open, addressed by row key
/// ([`RowLocator::key`]) + column name. At most one editor is open per
/// grid.
#[derive(Debug, Clone, PartialEq)]
struct ActiveEdit {
    row_key: String,
    column: String,
    /// Uncommitted editor text stashed when the open editor's row was
    /// scrolled out of the virtual window while its input didn't parse
    /// (FRE-74); the remounting editor seeds from it instead of the cell
    /// value.
    draft: Option<String>,
}

impl ActiveEdit {
    /// Whether this open editor is the one for `column` of `row_key`.
    ///
    /// The row half matters for the detail panel (FRE-109): its editor is
    /// owned above the row-keyed field list so a draft survives a row move,
    /// which means an editor left open on one row would otherwise reappear on
    /// a same-named field of the next one, seeded with the wrong row's text.
    fn is_on(&self, row_key: &str, column: &str) -> bool {
        self.row_key == row_key && self.column == column
    }
}

/// What the value-expand popup shows: a short value already in hand, or a
/// truncated cell whose full value is loaded lazily via
/// [`AppState::load_cell`] (FRE-33).
#[derive(Debug, Clone, PartialEq)]
enum ExpandView {
    /// A value already fully in the page — rendered directly, and safe to
    /// copy.
    ///
    /// Carries the [`Value`], not its display string: the popup shows
    /// `Value::display` (so a blob still reads as `<blob 12 B>`), but "Copy
    /// raw" has to put the same text on the clipboard as a Ctrl+C over that
    /// cell — [`raw_cell_text`], i.e. the blob's hex and nothing at all for
    /// NULL. Holding only the display string copied the placeholder.
    Text(Value),
    /// A truncated cell whose row can't be addressed (a view, a keyless
    /// table), so the full value can never be loaded. The popup shows the
    /// preview for reading but refuses to copy it — the same call
    /// [`plan_copy`] makes for this cell (FRE-110). Copying here would put a
    /// prefix plus a literal `…` on the clipboard.
    Truncated { display: String, column: String },
    /// A truncated cell: fetch and show its full value.
    Fetch { locator: RowLocator, column: String },
}

/// What Enter on a non-editable focused cell expands to: the value already in
/// hand, or — for a truncated preview — a fetch of the full value (FRE-33),
/// which needs the row to be addressable.
fn expand_view(cell: &GridNavCell, locator: Option<RowLocator>) -> ExpandView {
    match (cell.truncated(), locator) {
        (true, Some(locator)) => ExpandView::Fetch {
            locator,
            column: cell.column.clone(),
        },
        // Truncated but unaddressable: show the preview, but don't offer it
        // as a value to copy.
        (true, None) => ExpandView::Truncated {
            display: cell.display.clone(),
            column: cell.column.clone(),
        },
        (false, _) => ExpandView::Text(cell.value.clone()),
    }
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
    // The row detail panel's open editor (FRE-109). Lives here, above the
    // panel, because `RowDetailFields` is keyed by row: anything held inside
    // it is destroyed by every row move, which silently discarded whatever
    // the user had typed. Hoisting it is what lets the FRE-74 draft survive.
    let mut detail_editing = use_signal(|| Option::<ActiveEdit>::None);
    // One editor open anywhere, enforced in one place rather than at each of
    // the grid's six activation sites. Both editors render the same element
    // id, so two mounted at once fight over focus and leave one orphaned and
    // unreachable by Escape. The panel's own activation closes the grid's
    // directly; this closes the panel's whenever the grid opens one.
    //
    // It only ever *has* to act in one case, and that case is the whole
    // point. Any route to the grid blurs the panel's input first, which
    // commits and closes it — unless the text doesn't parse. So the panel
    // still being open here means unparseable text, which is exactly when its
    // `on_draft` would otherwise resurrect it on unmount. Clearing the signal
    // is what makes that stash see itself as stale and decline; the two are
    // load-bearing together, and neither works alone.
    use_effect(move || {
        if editing.read().is_some() && detail_editing.peek().is_some() {
            detail_editing.set(None);
        }
    });
    // Keyboard focus ring in the grid (row, col into the visible page's
    // rows × columns), and the value-expand popup (FRE-15).
    let mut focused_cell = use_signal(|| Option::<(usize, usize)>::None);
    // The other corner of a rectangular cell selection (FRE-110); the focus
    // ring is the near corner. `None` means the selection is exactly the
    // focused cell — the common case, and why this is a separate signal
    // rather than a rewrite of `focused_cell`: every existing focus behaviour
    // (clamping, scroll-into-view, Enter) keeps working untouched.
    let mut selection_anchor = use_signal(|| Option::<(usize, usize)>::None);
    // Outcome of the most recent copy, shown in the toolbar until the next
    // copy or a page/selection reset. Also carries a refusal (FRE-110).
    let mut copy_status = use_signal(|| Option::<CopyStatus>::None);
    // Whether the copy-as menu is open.
    let mut copy_menu = use_signal(|| false);
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

    // This grid's own refresh nonce, pulled out of the whole-map signal
    // through a memo: signal subscription is per-signal, not per-key, so a
    // raw `grid_refresh.read()` re-runs its readers on ANY write to the map —
    // a save bumping another table's nonce, or `close_connection` pruning a
    // closed tab's entries. The memo re-runs on those writes too, but its
    // PartialEq gate stops the propagation there: the reset effect and the
    // resources below only re-run when THIS grid's nonce actually changed
    // (FRE-129).
    let nonce_table_key = table.key();
    let refresh_nonce = use_memo(move || {
        state
            .grid_refresh
            .read()
            .get(&(id, nonce_table_key.clone()))
            .copied()
    });

    // Close any open editor and drop the row selection when the rows change
    // under them: a page flip, sort/filter change, or refetch replaces the
    // grid's contents — a stale ActiveEdit would spontaneously re-open the
    // editor if its row key scrolled back into view, and a stale selection
    // could stage deletes for rows the user no longer sees.
    use_effect(move || {
        let _ = page();
        let _ = sort.read();
        let _ = applied_filter.read();
        let _ = refresh_nonce();
        editing.set(None);
        selected.set(HashMap::new());
        // Re-seed the focus ring at the first cell and drop any expand popup;
        // the clamp effect below trims it to None for an empty page.
        focused_cell.set(Some((0, 0)));
        // …and collapse the cell selection to it (FRE-110): the rows under a
        // rectangle spanning the old page are not the rows the user picked.
        selection_anchor.set(None);
        copy_status.set(None);
        copy_menu.set(false);
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
    //
    // Derived through a memo because `registry` and `schemas` are whole-map
    // signals: a raw read in the resource would re-issue this grid's SQL
    // fetch on any other connection's open/close or schema load. The memo's
    // PartialEq gate lets the fetch re-run only when this table's own inputs
    // change (FRE-129).
    let meta_table = table.clone();
    let fetch_meta = use_memo(move || {
        let registry = state.registry.read();
        let schemas = state.schemas.read();
        let meta = find_table(schemas.get(&id), &meta_table);
        match (meta, registry.get(id)) {
            (Some(meta), Some(connection)) => {
                // Row identity is a read concern here — it decides which
                // columns must be fetched whole — so it comes from the
                // resolved access, not from whether editing is allowed.
                let identity = connection.pool.backend_row_identity(meta);
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
    });

    let table_for_resource = table.clone();
    let rows_resource = use_resource(move || {
        let table = table_for_resource.clone();
        // Read reactive deps before any await so the resource re-runs when
        // they change and no borrow spans the await.
        let (columns, no_preview, extra_key_column) = fetch_meta();
        let request = PageRequest {
            schema: table.schema.clone(),
            table: table.name.clone(),
            limit: PAGE_SIZE,
            offset: page() * PAGE_SIZE as u64,
            sort: sort(),
            filter: applied_filter(),
            extra_key_column,
        };
        let _ = refresh_nonce();
        // Peeked, not read: subscribing to `registry` here would re-fetch on
        // any connection open/close. The pool for a ConnectionId never
        // changes while this grid is mounted — the registry's only writes
        // are `insert` (mints a fresh id), `set_protection` (pool untouched)
        // and `remove` (which unmounts this tab) — so there is no pool
        // change to react to; a reconnect is a new id and a new grid.
        let pool = state.registry.peek().get(id).map(|c| c.pool.clone());
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
        let _ = refresh_nonce();
        // Peeked for the same reason as in `rows_resource`: the pool for a
        // ConnectionId is fixed for the connection's lifetime, and reading
        // `registry` would re-count on unrelated connection opens/closes.
        let pool = state.registry.peek().get(id).map(|c| c.pool.clone());
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

    // The introspected metadata every row of this table renders against
    // (FRE-130), built once here and handed to the row components as a single
    // pointer-compared prop instead of a `HashMap`/`Vec` deep-cloned into each
    // of the ~30 windowed rows on every render.
    //
    // A memo because `schemas` and `registry` are whole-map signals: a raw
    // read would rebuild — and re-render every row — on any other connection's
    // schema load. The memo re-runs then too, but its PartialEq gate stops the
    // propagation there (FRE-129's pattern).
    let render_table = table.clone();
    let render_meta = use_memo(move || {
        let schemas = state.schemas.read();
        let dialect = state.registry.read().get(id).map(|c| c.pool.dialect());
        Shared::new(TableRenderMeta::build(
            find_table(schemas.get(&id), &render_table),
            dialect,
        ))
    });

    // The fetched page reduced to what the grid renders (FRE-130): headers,
    // the rows with the stage applied, and the rows "select all on this page"
    // ticks.
    //
    // This is the grid's expensive derivation — it clones every cell `Value`
    // of the 100-row page. Deriving it in the render body re-ran it on every
    // arrow key, shift-click, copy and checkbox tick, because the render also
    // reads `focused_cell`, `selection`, `copy_status` and `selected`. Behind
    // a memo it re-runs only when the page, the stage or the table's resolved
    // access actually change; a focus move then costs the two `GridRow` diffs
    // it should.
    let page_table = table.clone();
    let page_view = use_memo(move || {
        let meta = render_meta();
        let current = rows_resource.read();
        let Some(Ok((page, extra_key))) = current.as_ref() else {
            return PageView::default();
        };
        let result = &page.result;
        // Same resolution the render uses, so keyboard navigation offers
        // exactly the editors the grid shows — the user's marking (FRE-111)
        // included. Resolving the *backend's* answer here instead would let
        // Enter open an editor on a cell the mouse correctly refuses.
        let access = find_table(state.schemas.read().get(&id), &page_table)
            .and_then(|meta| state.table_access(id, meta));
        let identity = access.as_ref().and_then(|a| a.identity.clone());
        let can_mutate = access.as_ref().is_some_and(TableAccess::can_mutate);
        let stage = state.table_stage(id, &page_table);
        // The fetch prepended the row-identity key column (rowid) when one was
        // requested; keep it for locators, hide it from display.
        let hidden = usize::from(extra_key.is_some());
        let headers: Vec<String> = if result.columns.is_empty() {
            meta.schema_columns.clone()
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
            can_mutate,
        );
        PageView {
            headers: Shared::new(headers),
            selectable: Shared::new(selectable_rows(&rows)),
            rows,
        }
    });

    // Keyboard-navigation model of the visible page (FRE-15): read by the grid
    // container's key handler for focus movement and Enter. Built from the
    // same `page_view` the render consumes — one row derivation, two readers —
    // but owning its data so the `'static` key closure can read it off-render.
    let grid_nav = use_memo(move || {
        let view = page_view.read();
        let meta = render_meta();
        GridNav::build((*view.headers).clone(), &view.rows, &meta.column_kinds)
    });

    // Keep the focus ring inside the current page and seed it once data
    // arrives, so it is visible and never indexes out of range after a page or
    // filter change shrinks the grid.
    // The selection's far corner is clamped alongside the focus (FRE-110): a
    // rectangle reaching past a page that just shrank would address rows the
    // grid no longer has.
    use_effect(move || {
        let (rows, cols) = grid_nav.read().dims();
        let focus = *focused_cell.peek();
        let anchor = *selection_anchor.peek();
        let current = match focus {
            Some(focus) => Selection {
                anchor: anchor.unwrap_or(focus),
                focus,
            },
            // Nothing focused yet: seed at the first cell if the page has one.
            None => Selection::single((0, 0)),
        };
        match current.clamped(rows, cols) {
            None => {
                if focus.is_some() {
                    focused_cell.set(None);
                }
                if anchor.is_some() {
                    selection_anchor.set(None);
                }
            }
            Some(clamped) => {
                if focus != Some(clamped.focus) {
                    focused_cell.set(Some(clamped.focus));
                }
                // Only re-pin an anchor that was actually set; a collapsed
                // selection must stay collapsed.
                if anchor.is_some() && anchor != Some(clamped.anchor) {
                    selection_anchor.set(Some(clamped.anchor));
                }
            }
        }
    });

    // The live cell selection (FRE-110): the focus ring's cell plus the
    // anchor, when one is pinned. `None` only for a page with nothing to
    // select. Derived rather than stored so the two corners can never drift
    // apart from the focus the rest of the grid navigates by.
    let selection = use_memo(move || {
        let focus = (*focused_cell.read())?;
        let anchor = selection_anchor.read().unwrap_or(focus);
        Some(Selection { anchor, focus })
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
    //
    // Both editors count (FRE-109): watching only the grid's left the arrow
    // keys dead after a commit in the panel, because focus stayed on the
    // closed panel editor's input and nothing handed it back.
    use_effect(move || {
        if editing.read().is_none() && detail_editing.read().is_none() {
            document::eval(
                "requestAnimationFrame(() => { \
                    const el = document.getElementById('dv-grid'); \
                    if (el) el.focus(); \
                });",
            );
        }
    });

    // The row detail panel's model (FRE-109). The panel's row is derived from
    // the selection's focus, never stored: it cannot drift from the cell the
    // grid has focused.
    //
    // Memoized (FRE-130): it clones the focused row's every value, and the
    // render body below re-runs on every copy, tick and selection change.
    // Reading `focused_cell` here is
    // deliberate — the panel follows the grid's focus — but the memo's
    // PartialEq gate means a move *within* one row (a left/right arrow)
    // rebuilds an equal detail and re-renders nothing. Shared so the panel's
    // prop stays pointer-comparable across the grid's own re-renders.
    let detail = use_memo(move || {
        // Nothing to derive while the panel is closed.
        if !state.row_detail_open(id) {
            return None;
        }
        row_detail(&grid_nav.read(), *focused_cell.read(), &render_meta()).map(Shared::new)
    });

    // Introspected metadata for this table (see [`TableRenderMeta`]): column
    // names feed the filter dropdown and the header fallback (so headers exist
    // even for zero-row results), foreign keys drive the clickable FK cells
    // (FRE-29), and the editor kinds decide what each cell may open.
    let meta = render_meta();
    // Row-identity detection decides the read-only notice and how staged rows
    // are addressed.
    let table_meta: Option<TableMeta> = find_table(state.schemas.read().get(&id), &table).cloned();
    // Whether the FK Back stack has anywhere to return to (reactive).
    let can_back = state.can_go_back(id);

    let dialect: Option<Dialect> = state.registry.read().get(id).map(|c| c.pool.dialect());
    // The connection's capabilities resolved for this table (FRE-87, narrowed
    // by the user's marking from FRE-111): one answer for whether editing is
    // possible, how rows are addressed, and — when it isn't possible — which
    // sentence explains it. Every editing affordance below gates on this
    // rather than re-deriving the rules.
    let access: Option<TableAccess> = table_meta
        .as_ref()
        .and_then(|meta| state.table_access(id, meta));
    let can_mutate = access.as_ref().is_some_and(TableAccess::can_mutate);
    // Stated up front, so the absent editors are explained rather than just
    // missing.
    let read_only_notice: Option<&'static str> =
        access.as_ref().and_then(TableAccess::read_only_notice);
    // A save parked on the FRE-111 confirmation, and the connection name it
    // has to state. Named rather than just "Are you sure?" — the state exists
    // to make you read which database you are about to change.
    let awaiting_confirm = state.save_awaiting_confirmation(id, &table.key());
    let connection_name: String = state
        .registry
        .read()
        .get(id)
        .map(|c| c.name.clone())
        .unwrap_or_default();

    // Staged (unsaved) changes of this table, if any.
    let stage: Option<TableStage> = state.table_stage(id, &table);
    let pending_count = stage.as_ref().map(TableStage::pending_count).unwrap_or(0);
    let saving = stage.as_ref().is_some_and(|s| s.saving);
    let save_error = stage.as_ref().and_then(|s| s.last_error.clone());
    // Insert/delete affordances only exist where editing works at all.
    let select_enabled = can_mutate;
    let missing_required = stage
        .as_ref()
        .map(|s| s.missing_required(&meta.required))
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
    let confirm_save_table = table.clone();
    let dismiss_save_table = table.clone();
    let row_table = table.clone();
    let detail_table = table.clone();
    // The row detail panel (FRE-109). Open/closed and width live on the tab,
    // not here, so they survive a table switch (which remounts this grid) —
    // and the open flag rides the persisted session.
    let detail_open = state.row_detail_open(id);
    let detail_width = clamp_detail_width(state.row_detail_width(id).unwrap_or(DETAIL_WIDTH));
    // Prev/Next in the panel move the GRID's focus, through the same
    // resolution an arrow key takes — the panel steers the one selection
    // rather than keeping a second row of its own.
    let on_detail_step = move |step: RowStep| {
        let (rows, cols) = grid_nav.peek().dims();
        if rows == 0 || cols == 0 {
            return;
        }
        let pos = focused_cell.peek().unwrap_or((0, 0));
        if let FocusOutcome::Cell(next) = apply_grid_move(pos, step.grid_move(), rows, cols) {
            // Collapses the selection, exactly like an unmodified arrow key.
            selection_anchor.set(None);
            focused_cell.set(Some(next));
            copy_status.set(None);
        }
    };
    // Everything a clipboard copy needs besides the selection itself
    // (FRE-110). Cloned per event handler; all of it is cheap.
    let copy_ctx = CopyContext {
        state,
        id,
        table: table.clone(),
        dialect,
        status: copy_status,
    };
    // Shape of the current selection — (rows, columns, cells) — for the Copy
    // button's label and tooltip.
    let selection_summary: Option<(usize, usize, usize)> = selection().map(|sel| {
        let (rows, cols) = sel.size();
        (rows, cols, sel.cell_count())
    });

    // Moves the focus ring to a clicked cell, extending the selection instead
    // of collapsing it when Shift is held (FRE-110). A run of shift-clicks all
    // extend from the same corner, because the anchor never moves.
    let on_select_cell = move |(row, col, shift): (usize, usize, bool)| {
        let focus = focused_cell.peek().unwrap_or((row, col));
        let current = Selection {
            anchor: selection_anchor.peek().unwrap_or(focus),
            focus,
        };
        let next = if shift {
            current.extended_to((row, col))
        } else {
            Selection::single((row, col))
        };
        selection_anchor.set(shift.then_some(next.anchor));
        focused_cell.set(Some(next.focus));
        copy_status.set(None);
    };

    // Grid keyboard navigation (FRE-15). Attached to the focusable scroll
    // container, so it also receives keydowns bubbling from focused children;
    // it no-ops while an editor is open (whose own keys bubble here). Movement
    // and Enter act on `grid_nav`/`focused_cell`; PageUp/PageDown flip pages.
    // Selection and copy (FRE-110) ride on the same model: Shift extends
    // instead of collapsing, Ctrl+A / Shift+Space / Ctrl+Space select the
    // page / row / column, and Ctrl+C copies.
    let key_copy_ctx = copy_ctx.clone();
    let on_grid_key = move |evt: KeyboardEvent| {
        if editing.peek().is_some() {
            return;
        }
        let code = evt.code();
        // Escape closes the value-expand popup, if one is open — and
        // otherwise collapses a multi-cell selection back to the focus.
        if code == Code::Escape {
            if expanded.peek().is_some() {
                evt.prevent_default();
                expanded.set(None);
            } else if selection_anchor.peek().is_some() {
                evt.prevent_default();
                selection_anchor.set(None);
            }
            return;
        }
        let modifiers = evt.modifiers();
        // Cmd on macOS is the same intent as Ctrl elsewhere for these.
        let ctrl = modifiers.ctrl() || modifiers.meta();
        let shift = modifiers.shift();
        let (rows, cols) = grid_nav.peek().dims();
        if rows == 0 || cols == 0 {
            return;
        }
        let pos = focused_cell.peek().unwrap_or((0, 0));
        let current = Selection {
            anchor: selection_anchor.peek().unwrap_or(pos),
            focus: pos,
        };
        // Copy (FRE-110): the plain shortcut, so `None` — a single cell
        // copies its raw value, a block copies TSV.
        if ctrl && code == Code::KeyC {
            evt.prevent_default();
            copy_menu.set(false);
            start_copy(&key_copy_ctx, &grid_nav.peek(), current, None);
            return;
        }
        // Select all / whole row / whole column. Ctrl+A must also suppress
        // the webview's own "select the page's text".
        let axis = match code {
            Code::KeyA if ctrl => Selection::all(rows, cols),
            Code::Space if ctrl => Selection::column(pos.1, rows),
            Code::Space if shift => Selection::row(pos.0, cols),
            _ => None,
        };
        if let Some(axis) = axis {
            evt.prevent_default();
            selection_anchor.set(Some(axis.anchor));
            focused_cell.set(Some(axis.focus));
            copy_status.set(None);
            return;
        }
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
                        draft: None,
                    })),
                    _ => expanded.set(Some(expand_view(cell, nav.rows[r].locator.clone()))),
                }
            }
            return;
        }
        let Some(mv) = grid_move_for(code, ctrl) else {
            return;
        };
        evt.prevent_default();
        match apply_grid_move(pos, mv, rows, cols) {
            FocusOutcome::Cell(next) => {
                // Shift extends the rectangle (the anchor stays where the
                // selection started); an unmodified move collapses it.
                let moved = current.extended_to(next);
                selection_anchor.set(shift.then_some(moved.anchor));
                focused_cell.set(Some(moved.focus));
            }
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
                    for column in meta.schema_columns.iter() {
                        option {
                            value: "{column}",
                            selected: *filter_column.read() == *column,
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
                if let Some(status) = copy_status.read().as_ref() {
                    span {
                        class: "min-w-0 truncate text-xs {status.class()}",
                        title: "{status.text}",
                        "{status.text}"
                    }
                }
                if let Some(status) = export_status.as_ref() {
                    {
                        let (text, class) = status.line();
                        rsx! { span { class: "truncate text-xs {class}", title: "{text}", "{text}" } }
                    }
                }
                // Copy-as menu (FRE-110). Copies the selected cells only —
                // the Export buttons beside it cover the whole view.
                if let Some((sel_rows, sel_cols, sel_cells)) = selection_summary {
                    div { class: "relative shrink-0",
                        button {
                            class: "rounded px-2 py-1 text-xs text-slate-500 dark:text-slate-400 hover:bg-slate-200 dark:hover:bg-slate-800 hover:text-slate-900 dark:hover:text-slate-100",
                            title: "Copy the {sel_cells} selected cell(s) — Ctrl+C copies TSV",
                            onclick: move |_| {
                                let open = *copy_menu.peek();
                                copy_menu.set(!open);
                            },
                            "Copy {sel_rows}×{sel_cols} ▾"
                        }
                        if copy_menu() {
                            // Full-screen catcher: a click anywhere else
                            // closes the menu (no window-level listener).
                            div {
                                class: "fixed inset-0 z-30",
                                onclick: move |_| copy_menu.set(false),
                            }
                            div { class: "absolute right-0 z-40 mt-1 w-44 overflow-hidden rounded border border-slate-300 dark:border-slate-700 bg-white dark:bg-slate-900 py-1 shadow-lg",
                                for format in COPY_FORMATS {
                                    // INSERT is offered only when the dialect
                                    // is known, mirroring the Export buttons'
                                    // gate — `start_copy` refuses it anyway,
                                    // but an absent entry beats a failing one.
                                    if format != CopyFormat::Insert || dialect.is_some() {
                                        button {
                                            key: "{format.label()}",
                                            class: "block w-full px-3 py-1 text-left text-xs text-slate-700 dark:text-slate-300 hover:bg-slate-200 dark:hover:bg-slate-800",
                                            onclick: {
                                                let ctx = copy_ctx.clone();
                                                move |_| {
                                                    copy_menu.set(false);
                                                    if let Some(selection) = *selection.peek() {
                                                        start_copy(&ctx, &grid_nav.peek(), selection, Some(format));
                                                    }
                                                }
                                            },
                                            "{format.label()}"
                                        }
                                    }
                                }
                            }
                        }
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
                // Row detail (FRE-109): the same toggle as Ctrl+D, here so the
                // panel is discoverable without knowing the shortcut.
                button {
                    class: if detail_open {
                        "flex items-center gap-1 rounded bg-slate-300 dark:bg-slate-700 px-2 py-1 text-xs text-slate-900 dark:text-slate-100"
                    } else {
                        "flex items-center gap-1 rounded px-2 py-1 text-xs text-slate-500 dark:text-slate-400 hover:bg-slate-200 dark:hover:bg-slate-800 hover:text-slate-900 dark:hover:text-slate-100"
                    },
                    title: "Show the focused row as a form (Ctrl+D)",
                    onclick: move |_| state.set_row_detail(id, !detail_open),
                    PanelRight { size: 12 }
                    "Row detail"
                }
                button {
                    class: "flex items-center gap-1 rounded px-2 py-1 text-xs text-slate-500 dark:text-slate-400 hover:bg-slate-200 dark:hover:bg-slate-800 hover:text-slate-900 dark:hover:text-slate-100",
                    title: "Re-run the current query",
                    onclick: move |_| state.bump_grid_refresh(id, &refresh_table.key()),
                    RefreshCw { size: 12 }
                    "Refresh"
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
                    if let Some(notice) = read_only_notice {
                        span { class: "text-red-600 dark:text-red-300", "{notice}" }
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
                        disabled: save_disabled(saving, missing_required, can_mutate),
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
            // Write-protection confirmation (FRE-111): the connection is
            // marked Confirm, so the apply waits here until the user has read
            // which database it targets. The changes stay staged meanwhile —
            // dismissing costs nothing.
            if awaiting_confirm {
                div { class: "flex items-center gap-3 border-b border-red-400 dark:border-red-800/60 bg-red-100 dark:bg-red-950/40 px-3 py-1.5 text-xs",
                    ShieldAlert { size: 14 }
                    span { class: "min-w-0 flex-1 text-red-700 dark:text-red-200",
                        if pending_count == 1 {
                            "Apply 1 change to \"{connection_name}\"? This connection is marked to confirm writes."
                        } else {
                            "Apply {pending_count} changes to \"{connection_name}\"? This connection is marked to confirm writes."
                        }
                    }
                    button {
                        class: "rounded bg-red-700 px-3 py-1 font-semibold text-white hover:bg-red-600",
                        onclick: move |_| state.confirm_pending_save(id, &confirm_save_table),
                        "Apply to \"{connection_name}\""
                    }
                    button {
                        class: "rounded border border-slate-400 dark:border-slate-600 px-3 py-1 text-slate-900 dark:text-slate-300 hover:bg-slate-200 dark:hover:bg-slate-800",
                        onclick: move |_| state.dismiss_pending_save(id, &dismiss_save_table),
                        "Cancel"
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
            // The rows and, docked to their right, the row detail panel
            // (FRE-109). The toolbar and footer above and below span both:
            // they describe the table, while the panel describes one row.
            div { class: "flex min-h-0 flex-1",
                // Grid — a single focusable region (tabindex 0) so arrow-key cell
                // navigation works without per-cell tab stops (FRE-15). Focused on
                // mount so the ring responds immediately; `outline-none` since the
                // ring itself signals focus.
                div {
                    id: "dv-grid",
                    // `select-none` (FRE-110): shift-click extends the *cell*
                    // selection, and without this the webview drags its own text
                    // highlight across the rows at the same time, striping the
                    // grid. The expand popup renders outside this container, so
                    // its text stays selectable.
                    class: "min-h-0 flex-1 select-none overflow-auto outline-none",
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
                        Some(Ok(_)) => {
                            // Rows, headers and the selectable set all come from
                            // the `page_view` memo (FRE-130) — this arm no longer
                            // re-derives them from the resource, so a focus move
                            // or a copy costs nothing here.
                            let view = page_view.read();
                            let headers = view.headers.clone();
                            let selectable = view.selectable.clone();
                            let pending_inserts: Vec<PendingInsert> = stage
                                .as_ref()
                                .map(|s| s.inserts().to_vec())
                                .unwrap_or_default();
                            let all_selected = !selectable.is_empty() && {
                                let sel = selected.read();
                                selectable.iter().all(|(key, _)| sel.contains_key(key))
                            };
                            let insert_parent_table = row_table.clone();
                            let new_row_table = row_table.clone();
                            let empty = empty_state(
                                view.rows.is_empty() && pending_inserts.is_empty(),
                                applied_filter.read().is_some(),
                            );
                            // Windowed rendering (FRE-32): only rows in the visible
                            // range go in the DOM; a top and bottom spacer row of
                            // the elided rows' total height keeps the scrollbar and
                            // offsets correct. `total_cols` sizes the spacers'
                            // single colspan cell across every column.
                            let total_rows = view.rows.len();
                            let total_cols = headers.len() + usize::from(select_enabled);
                            let (win_start, win_end) = *visible_range.read();
                            let (win_start, win_end) = (win_start.min(total_rows), win_end.min(total_rows));
                            let top_spacer = win_start as f64 * ROW_HEIGHT;
                            let bottom_spacer = (total_rows - win_end) as f64 * ROW_HEIGHT;
                            // Only the window's rows are cloned out of the memo;
                            // the rest of the page stays in it (FRE-130).
                            let windowed_rows = window_rows(&view.rows, win_start, win_end);
                            drop(view);
                            rsx! {
                                match empty {
                                    // No-filter-match: distinct from an empty
                                    // table, with a Clear-filter action.
                                    Some(GridEmpty::NoMatch) => rsx! {
                                        EmptyState {
                                            icon: rsx! { SearchX { size: 40 } },
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
                                            icon: rsx! { File { size: 40 } },
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
                                                                    for (key, locator) in selectable.iter() {
                                                                        map.insert(key.clone(), locator.clone());
                                                                    }
                                                                    selected.set(map);
                                                                }
                                                            },
                                                        }
                                                    }
                                                }
                                                for (col_index , header) in headers.iter().cloned().enumerate() {
                                                    GridHeader {
                                                        name: header,
                                                        sort: sort_value.clone(),
                                                        on_sort: move |name: String| {
                                                            let next = next_sort(&sort.peek(), &name);
                                                            sort.set(next);
                                                            page.set(0);
                                                        },
                                                        // Shift-click selects the whole column (FRE-110);
                                                        // a plain click keeps sorting.
                                                        on_select_column: move |_| {
                                                            let rows = grid_nav.peek().rows.len();
                                                            if let Some(axis) = Selection::column(col_index, rows) {
                                                                selection_anchor.set(Some(axis.anchor));
                                                                focused_cell.set(Some(axis.focus));
                                                                copy_status.set(None);
                                                            }
                                                        },
                                                    }
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
                                                    meta: meta.clone(),
                                                    dialect: dialect.unwrap_or(Dialect::Sqlite),
                                                    editing,
                                                    // The focused column in this row (FRE-15), else None; only the
                                                    // two rows whose focus changed re-render on a move.
                                                    focused_col: match focused_cell() {
                                                        Some((r, c)) if r == index => Some(c),
                                                        _ => None,
                                                    },
                                                    row_index: index,
                                                    // The inclusive span of selected columns in this row
                                                    // (FRE-110), or None when the row is outside the
                                                    // rectangle — so only rows whose span changed re-render.
                                                    selected_cols: selection().and_then(|sel| sel.columns_in(index)),
                                                    on_select_cell,
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
                                                    headers: headers.clone(),
                                                    meta: meta.clone(),
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
                if detail_open {
                    RowDetailPanel {
                        id,
                        table: detail_table,
                        detail: detail(),
                        width: detail_width,
                        dialect: dialect.unwrap_or(Dialect::Sqlite),
                        read_only_notice: read_only_notice.map(str::to_string),
                        grid_editing: editing,
                        editing: detail_editing,
                        on_step: on_detail_step,
                        on_close: move |_| state.set_row_detail(id, false),
                        on_width: move |width: f64| state.set_row_detail_width(id, width),
                        on_fk_jump: {
                            let origin = row_table.clone();
                            move |(fk, row_values): (ForeignKeyMeta, HashMap<String, Value>)| {
                                state.navigate_fk(id, &fk, &row_values, &origin, applied_filter.peek().clone());
                            }
                        },
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
                                X { size: 16 }
                            }
                        }
                        match view {
                            // A short value already in hand is always the full
                            // value, so JSON pretty-printing is safe (FRE-77).
                            ExpandView::Text(value) => {
                                let shown = value.display();
                                let display = pretty_json(&shown).unwrap_or_else(|| shown.clone());
                                rsx! {
                                    CopyRawButton { raw: raw_cell_text(&value) }
                                    pre { class: "whitespace-pre-wrap break-words font-mono text-xs text-slate-900 dark:text-slate-200",
                                        "{display}"
                                    }
                                }
                            },
                            // Preview of an unloadable value: readable, but no
                            // Copy raw — it would put a prefix on the clipboard
                            // (FRE-110). Not pretty-printed either; the text is
                            // cut mid-document.
                            ExpandView::Truncated { display, column } => rsx! {
                                Banner {
                                    kind: BannerKind::Warning,
                                    message: CopyRefusal::Unaddressable { column }.message(),
                                }
                                pre { class: "mt-2 whitespace-pre-wrap break-words font-mono text-xs text-slate-900 dark:text-slate-200",
                                    "{display}"
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

/// The fetched page reduced to exactly what the grid renders (FRE-130).
///
/// Built once per page/stage change by a memo and read by both the render and
/// the keyboard-navigation model, so the page is never re-derived by a focus
/// move, a copy or a checkbox tick. Structurally `PartialEq` (rather than
/// pointer-compared) because that comparison is what gates those readers: a
/// rebuild from unchanged inputs must re-render nothing.
#[derive(Debug, Default, Clone, PartialEq)]
struct PageView {
    /// Visible column names: the fetched result's, or the schema's when a
    /// zero-row page carries none. [`Shared`] because the header list is also
    /// handed to every pending-insert row.
    headers: Shared<Vec<String>>,
    /// The page's rows with the stage applied (see [`view_rows`]).
    rows: Vec<RowView>,
    /// The rows "select all on this page" ticks (see [`selectable_rows`]).
    /// [`Shared`] because the checkbox handler has to own a copy.
    selectable: Shared<Vec<(String, RowLocator)>>,
}

/// This page's selectable rows, in page order: those that are addressable and
/// not already pending delete. Backs both the header checkbox's ticked state
/// and what clicking it selects.
fn selectable_rows(rows: &[RowView]) -> Vec<(String, RowLocator)> {
    rows.iter()
        .filter(|row| !row.deleted)
        .filter_map(|row| Some((row.key.clone()?, row.locator.clone()?)))
        .collect()
}

/// The `[start, end)` slice of the page to put in the DOM (FRE-32), each row
/// paired with its index on the **whole page** — not its position in the
/// window. Those indices are what the focus ring, the selection rectangle and
/// the click handler address rows by, so they must survive the slicing.
///
/// Only these rows are cloned out of the [`PageView`] memo (FRE-130); the rest
/// of the page is never copied per render. `start`/`end` are clamped by the
/// caller against the page length.
fn window_rows(rows: &[RowView], start: usize, end: usize) -> Vec<(usize, RowView)> {
    rows[start..end]
        .iter()
        .cloned()
        .enumerate()
        .map(|(offset, row)| (start + offset, row))
        .collect()
}

/// Everything the grid's rows render against that comes from introspection
/// rather than from the fetched page (FRE-130).
///
/// Bundled into one value so it can be shared by pointer: as separate props
/// each of the ~30 windowed rows deep-cloned a `HashMap` and a `Vec` per
/// render, and Dioxus then deep-compared them to decide the row hadn't
/// changed. It changes only with the schema, so a memo rebuilds it far less
/// often than the grid re-renders.
#[derive(Debug, Default, PartialEq)]
struct TableRenderMeta {
    /// The table's column names in schema order: the filter dropdown's options
    /// and the header fallback for a page with no rows to name them.
    schema_columns: Vec<String>,
    /// Full column metadata, for the detail panel's declared types.
    columns: Vec<ColumnMeta>,
    /// Per-column editor kind + nullability (see [`column_kinds_of`]).
    column_kinds: HashMap<String, (EditorKind, bool)>,
    /// Foreign keys of this table, indexed by `col_to_fk` (FRE-29).
    foreign_keys: Vec<ForeignKeyMeta>,
    /// Referencing column → index into `foreign_keys`; a column in several FKs
    /// takes the first (documented v1 limit).
    col_to_fk: HashMap<String, usize>,
    /// Required-column flagging for pending inserts: NOT NULL + no default +
    /// not auto-assigned (see [`required_insert_columns`] for the per-backend
    /// rules). Unfilled required cells red-flag and block Save.
    required: HashSet<String>,
}

impl TableRenderMeta {
    /// Reduces one table's introspected metadata to what rendering needs.
    /// `dialect` only feeds the required-column rules, which are per-backend;
    /// without a live connection nothing is flagged (there is nothing to save
    /// through either). Empty when the schema isn't loaded yet.
    fn build(meta: Option<&TableMeta>, dialect: Option<Dialect>) -> Self {
        let Some(meta) = meta else {
            return TableRenderMeta::default();
        };
        let mut col_to_fk = HashMap::new();
        for (index, fk) in meta.foreign_keys.iter().enumerate() {
            for column in &fk.columns {
                col_to_fk.entry(column.clone()).or_insert(index);
            }
        }
        TableRenderMeta {
            schema_columns: meta.columns.iter().map(|c| c.name.clone()).collect(),
            columns: meta.columns.clone(),
            column_kinds: column_kinds_of(Some(meta)),
            foreign_keys: meta.foreign_keys.clone(),
            col_to_fk,
            required: dialect
                .map(|dialect| required_insert_columns(meta, dialect))
                .unwrap_or_default(),
        }
    }

    /// Editor kind + nullability for one column; see [`column_kind`].
    fn kind_of(&self, column: &str) -> (EditorKind, bool) {
        column_kind(column, &self.column_kinds)
    }

    /// The foreign key this column belongs to, when following it leads
    /// somewhere: a NULL key references nothing (FRE-29).
    fn fk_of(&self, column: &str, value: &Value) -> Option<&ForeignKeyMeta> {
        if value.is_null() {
            return None;
        }
        self.col_to_fk
            .get(column)
            .and_then(|&index| self.foreign_keys.get(index))
    }
}

/// Applies the stage to the fetched page: computes each row's locator from
/// the identity's key columns (matched by name against the result), then
/// substitutes staged cell values (dirty) and flags pending deletes. Rows
/// whose key columns are missing from the result (transient schema/result
/// mismatch) render clean and read-only — they can't be addressed.
///
/// `can_mutate` is the table's resolved write capability (FRE-87). Locators
/// are still built when it is false: addressing a row is a read concern too
/// (cell expand fetches a single value through the same key), so a read-only
/// connection keeps working — only `editable` turns off.
fn view_rows(
    result: &QueryResult,
    previews: &[Vec<Option<PreviewInfo>>],
    hidden: usize,
    identity: Option<&RowIdentity>,
    stage: Option<&TableStage>,
    can_mutate: bool,
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
                    editable: can_mutate
                        && locator.is_some()
                        && !deleted
                        && !matches!(value, Value::Blob(_)),
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

/// Save is blocked while a save is in flight, while any pending insert still
/// lacks a required column, or when the table's resolved capabilities forbid
/// writing (FRE-87). The last case shouldn't arise — changes can only be
/// staged where cells are editable — but a Save button that can only fail is
/// worse than a disabled one carrying the reason.
fn save_disabled(saving: bool, missing_required: usize, can_mutate: bool) -> bool {
    saving || missing_required > 0 || !can_mutate
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
                    editor_kind(&c.type_name, &c.type_detail)
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
    editable_for_kind(cell, &cell_kind(cell, column_kinds).0)
}

/// [`cell_editable`] for a cell whose column kind has already been resolved —
/// the row renderer looks each column up once and asks this (FRE-130), rather
/// than repeating the lookup per question.
fn editable_for_kind(cell: &CellView, kind: &EditorKind) -> bool {
    cell.editable && !kind.is_read_only()
}

/// Editor kind + nullability for one column; columns missing from the
/// introspected metadata edit as nullable text.
fn column_kind(
    column: &str,
    column_kinds: &HashMap<String, (EditorKind, bool)>,
) -> (EditorKind, bool) {
    column_kinds
        .get(column)
        .cloned()
        .unwrap_or((EditorKind::Text, true))
}

/// [`column_kind`] for a prepared cell.
fn cell_kind(
    cell: &CellView,
    column_kinds: &HashMap<String, (EditorKind, bool)>,
) -> (EditorKind, bool) {
    column_kind(&cell.column, column_kinds)
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
    /// Whether the stage holds an edit for this cell — the row detail panel
    /// (FRE-109) tints its fields from the same snapshot the grid tints its
    /// cells from, so the two can't disagree about what is staged.
    dirty: bool,
    display: String,
    /// The cell's value as fetched. For a cell carrying `preview` this is only
    /// the bounded prefix — a copy must fetch the full value (FRE-110).
    value: Value,
    /// Full-value metadata when this cell is a truncated preview; drives both
    /// the expand-on-Enter fetch (FRE-33) and the copy's fetch/refusal
    /// decision (FRE-110).
    preview: Option<PreviewInfo>,
}

impl GridNavCell {
    /// Whether this cell is a truncated preview — Enter expands it by fetching
    /// the full value rather than showing the in-hand preview (FRE-33).
    fn truncated(&self) -> bool {
        self.preview.is_some()
    }
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
                        dirty: cell.dirty,
                        display: cell_display(cell),
                        value: cell.value.clone(),
                        preview: cell.preview,
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

/// Default width of the row detail panel in CSS pixels, and the range a drag
/// may take it to (FRE-109). The floor keeps a name/type header legible; the
/// ceiling keeps the grid — which the panel accompanies rather than replaces
/// — from being squeezed away.
const DETAIL_WIDTH: f64 = 360.0;
const DETAIL_MIN_WIDTH: f64 = 240.0;
const DETAIL_MAX_WIDTH: f64 = 720.0;

/// Clamps a dragged panel width into the allowed range. A non-finite width
/// (a nonsense report from the drag listener) falls back to the default
/// rather than propagating a NaN into the style attribute.
fn clamp_detail_width(width: f64) -> f64 {
    if !width.is_finite() {
        return DETAIL_WIDTH;
    }
    width.clamp(DETAIL_MIN_WIDTH, DETAIL_MAX_WIDTH)
}

/// One field of the row detail panel (FRE-109): a column of the focused row,
/// with everything the panel needs to show it, edit it, and follow it.
#[derive(Debug, Clone, PartialEq)]
struct DetailField {
    column: String,
    /// Declared type, shown beside the name — via the Schema pane's
    /// [`display_type`], so the two views never disagree about what a column
    /// is (a Postgres enum reads as its type, not `USER-DEFINED`).
    type_name: String,
    /// The cell as the grid holds it: for a previewed cell only the bounded
    /// prefix, which the panel replaces with the full value (FRE-33).
    value: Value,
    preview: Option<PreviewInfo>,
    dirty: bool,
    kind: EditorKind,
    nullable: bool,
    /// The grid's own answer (`GridNavCell::editable`), which already folds in
    /// the resolved capability and the user's marking (FRE-87/FRE-111) — not
    /// a second resolution that could disagree with the cell beside it.
    editable: bool,
    /// The foreign key this column belongs to, when following it leads
    /// somewhere: a NULL key references nothing (FRE-29).
    fk: Option<ForeignKeyMeta>,
}

impl DetailField {
    /// Whether this field's full value is past [`FETCH_CELL_MAX_BYTES`], so it
    /// will render a note rather than an editor.
    ///
    /// Read off the preview the page already carries rather than recomputed,
    /// and known before the fetch resolves — the same answer
    /// [`CellFetch::capped`](crate::db::CellFetch) gives afterwards.
    fn over_fetch_cap(&self) -> bool {
        self.preview
            .as_ref()
            .is_some_and(|preview| preview.full_len > FETCH_CELL_MAX_BYTES as u64)
    }
}

/// Where the focused row sits on the page — the panel's header line and its
/// Prev/Next bounds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct DetailPosition {
    /// 1-based position of the row within the fetched page.
    number: usize,
    total: usize,
}

impl DetailPosition {
    fn has_prev(&self) -> bool {
        self.number > 1
    }

    /// Prev/Next stay inside the page, like the arrow keys they delegate to:
    /// paging is PageUp/PageDown's job, and a silent page flip under an open
    /// form would be a surprise.
    fn has_next(&self) -> bool {
        self.number < self.total
    }
}

/// The focused row, reduced to what the row detail panel renders.
#[derive(Debug, Clone, PartialEq)]
struct RowDetail {
    /// Identifies the row across renders so the panel's fields — each owning
    /// a full-value fetch — remount when the focus moves to another row.
    row_key: String,
    /// `None` for a row that can't be addressed (a view, a keyless table):
    /// nothing can be staged for it and its previews can't be loaded.
    locator: Option<RowLocator>,
    fields: Vec<DetailField>,
    /// The whole row by column — the source of an FK jump's equality filter.
    /// [`Shared`] because every field of the panel carries it (FRE-130).
    row_values: Shared<HashMap<String, Value>>,
    position: DetailPosition,
}

/// Reduces the focused row of `nav` to the panel's model (FRE-109).
///
/// The row is taken from the grid's focus (the selection model's focus
/// corner), not from any row the panel remembers for itself — and the panel's
/// Prev/Next move that same focus. One row, one place it lives.
///
/// `None` when there is nothing to show (an empty page).
fn row_detail(
    nav: &GridNav,
    focused: Option<(usize, usize)>,
    meta: &TableRenderMeta,
) -> Option<RowDetail> {
    // No focus yet means the page just arrived and the clamp effect is about
    // to seed one at (0, 0) — show that row now rather than flashing empty.
    let index = focused.map_or(0, |(row, _)| row);
    let row = nav.rows.get(index)?;
    let types: HashMap<&str, String> = meta
        .columns
        .iter()
        .map(|column| (column.name.as_str(), display_type(column)))
        .collect();
    let mut fields = Vec::with_capacity(row.cells.len());
    let mut row_values = HashMap::with_capacity(row.cells.len());
    for cell in &row.cells {
        let (kind, nullable) = meta.kind_of(&cell.column);
        fields.push(DetailField {
            type_name: types.get(cell.column.as_str()).cloned().unwrap_or_default(),
            fk: meta.fk_of(&cell.column, &cell.value).cloned(),
            column: cell.column.clone(),
            value: cell.value.clone(),
            preview: cell.preview,
            dirty: cell.dirty,
            editable: cell.editable,
            kind,
            nullable,
        });
        row_values.insert(cell.column.clone(), cell.value.clone());
    }
    Some(RowDetail {
        // A row without a locator still needs a stable identity for keying;
        // its position on the page is the only one available.
        row_key: row.key.clone().unwrap_or_else(|| format!("#{index}")),
        locator: row.locator.clone(),
        fields,
        row_values: Shared::new(row_values),
        position: DetailPosition {
            number: index + 1,
            total: nav.rows.len(),
        },
    })
}

/// Which way the panel's Prev/Next moves the grid's focused row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RowStep {
    Prev,
    Next,
}

impl RowStep {
    /// The grid move this step delegates to, so a panel step and an arrow key
    /// resolve through exactly the same bounds logic.
    fn grid_move(self) -> GridMove {
        match self {
            RowStep::Prev => GridMove::Up,
            RowStep::Next => GridMove::Down,
        }
    }
}

/// The clipboard formats in copy-as menu order (FRE-110). TSV leads: it is
/// what the plain shortcut produces and what spreadsheets want.
const COPY_FORMATS: [CopyFormat; 6] = [
    CopyFormat::Tsv { header: false },
    CopyFormat::Tsv { header: true },
    CopyFormat::Csv,
    CopyFormat::Json,
    CopyFormat::Insert,
    CopyFormat::Markdown,
];

/// Outcome of the most recent copy, shown as a toolbar line (FRE-110). It
/// stays until the next copy or a selection/page change, mirroring how the
/// export status behaves.
#[derive(Debug, Clone, PartialEq)]
struct CopyStatus {
    text: String,
    error: bool,
}

impl CopyStatus {
    fn ok(text: String) -> Self {
        CopyStatus { text, error: false }
    }

    fn failed(text: String) -> Self {
        CopyStatus { text, error: true }
    }

    fn class(&self) -> &'static str {
        if self.error {
            "text-red-600 dark:text-red-400"
        } else {
            "text-emerald-700 dark:text-emerald-400"
        }
    }
}

/// Everything a copy needs besides the selection: which connection and table
/// the rows came from, the dialect to render SQL literals for, and where to
/// report the outcome.
#[derive(Clone)]
struct CopyContext {
    state: AppState,
    id: ConnectionId,
    table: TableRef,
    /// `None` when the connection is gone. Only [`CopyFormat::Insert`] needs
    /// it, and that format refuses rather than assuming a dialect — see
    /// [`CopyRefusal::UnknownDialect`].
    dialect: Option<Dialect>,
    status: Signal<Option<CopyStatus>>,
}

/// One cell of a planned copy: a value already held in full, or a ticket to
/// load one the grid holds only a bounded preview of (FRE-33). Copying a
/// preview would put silently truncated data on the clipboard, which is the
/// one thing FRE-110 must not do.
#[derive(Debug, Clone, PartialEq)]
enum CopyCell {
    Ready(Value),
    Fetch { locator: RowLocator, column: String },
}

/// A copy reduced to what it needs: the selected column names and the
/// selected cells, row-major.
#[derive(Debug, Clone, PartialEq)]
struct CopyPlan {
    columns: Vec<String>,
    rows: Vec<Vec<CopyCell>>,
}

/// Why a copy was refused outright (FRE-110). Refusing beats truncating: a
/// truncated INSERT is still valid SQL and will run, writing wrong data with
/// no error anywhere.
#[derive(Debug, Clone, PartialEq)]
enum CopyRefusal {
    /// A selected cell is bigger than what a cell fetch can load.
    TooLarge { column: String, full_len: u64 },
    /// A selected cell is only a preview and its row has no locator (a view,
    /// or a keyless table), so the full value can't be loaded at all.
    Unaddressable { column: String },
    /// The connection's SQL dialect is unknown (the tab is closing), so
    /// INSERT statements can't be rendered. Refusing beats defaulting to one:
    /// a guessed dialect emits SQL that parses and runs in the wrong flavour,
    /// which is the silent-wrongness this format is guarded against.
    UnknownDialect,
}

impl CopyRefusal {
    /// The toolbar line: names the offending column and the cap, and points
    /// at the export, which streams and has no such limit.
    fn message(&self) -> String {
        match self {
            CopyRefusal::TooLarge { column, full_len } => format!(
                "Can't copy: \"{column}\" holds {}, over the {} copy limit. Use Export for values this large.",
                human_bytes(*full_len),
                human_bytes(FETCH_CELL_MAX_BYTES as u64),
            ),
            CopyRefusal::Unaddressable { column } => format!(
                "Can't copy: \"{column}\" is truncated and this table's rows can't be addressed to load the full value. Use Export instead."
            ),
            CopyRefusal::UnknownDialect => {
                "Can't copy as INSERT: this connection's SQL dialect is unknown.".to_string()
            }
        }
    }
}

/// Reduces a selection over the visible page to a [`CopyPlan`], or refuses it
/// (FRE-110). Pure — the async value loading happens in [`start_copy`].
///
/// Cells outside the page (a selection racing a shrinking page) are skipped
/// rather than erroring; the clamp effect normally prevents that.
///
/// The `full_len` compared against the byte cap here is in *characters* for
/// text. That is not a conservative approximation — characters ≤ bytes, so as
/// a byte test it is permissive, and a text copy can exceed 8 MB of actual
/// bytes. What makes it correct is that it is not really a byte test: the
/// backend measures a value and slices it in the **same unit**
/// (`substr`/`length` on SQLite, `left`/`length` on Postgres,
/// `SUBSTRING`/`DATALENGTH … / 2` on SQL Server), so `full_len > cap` means
/// exactly "the fetch would truncate this". Keeping those two in step is the
/// whole invariant — see [`sql::mssql_text_len`](crate::db) for the one place
/// it was broken.
fn plan_copy(nav: &GridNav, selection: Selection) -> Result<CopyPlan, CopyRefusal> {
    let rect = selection.bounds();
    let columns: Vec<String> = (rect.left..=rect.right)
        .filter_map(|col| nav.headers.get(col).cloned())
        .collect();
    let mut rows = Vec::new();
    for row_index in rect.top..=rect.bottom {
        let Some(row) = nav.rows.get(row_index) else {
            continue;
        };
        let mut cells = Vec::new();
        for col in rect.left..=rect.right {
            let Some(cell) = row.cells.get(col) else {
                continue;
            };
            let Some(preview) = cell.preview else {
                cells.push(CopyCell::Ready(cell.value.clone()));
                continue;
            };
            if preview.full_len > FETCH_CELL_MAX_BYTES as u64 {
                return Err(CopyRefusal::TooLarge {
                    column: cell.column.clone(),
                    full_len: preview.full_len,
                });
            }
            match row.locator.clone() {
                Some(locator) => cells.push(CopyCell::Fetch {
                    locator,
                    column: cell.column.clone(),
                }),
                None => {
                    return Err(CopyRefusal::Unaddressable {
                        column: cell.column.clone(),
                    })
                }
            }
        }
        rows.push(cells);
    }
    Ok(CopyPlan { columns, rows })
}

/// Copies `selection` to the clipboard in `format` — or, for `None` (the
/// plain Ctrl+C shortcut), as the raw value of a single cell and TSV for a
/// block (FRE-110).
///
/// Plans synchronously so an oversize selection is refused before anything
/// runs, then resolves any previewed cells to their full values in a spawned
/// task. No signal borrow crosses an await: `load_cell` clones the pool and
/// metadata out of the signals before it awaits, and the plan is owned.
fn start_copy(ctx: &CopyContext, nav: &GridNav, selection: Selection, format: Option<CopyFormat>) {
    let mut status = ctx.status;
    let plan = match plan_copy(nav, selection) {
        Ok(plan) => plan,
        Err(refusal) => {
            status.set(Some(CopyStatus::failed(refusal.message())));
            return;
        }
    };
    let (format, raw) = match format {
        Some(format) => (format, false),
        None => (CopyFormat::Tsv { header: false }, selection.is_single()),
    };
    let (state, id, dialect) = (ctx.state, ctx.id, ctx.dialect);
    // Refuse before fetching anything: INSERT is the one format that needs a
    // dialect, and it must never fall back to one.
    if format == CopyFormat::Insert && dialect.is_none() {
        status.set(Some(CopyStatus::failed(
            CopyRefusal::UnknownDialect.message(),
        )));
        return;
    }
    let table = ctx.table.clone();
    spawn(async move {
        let mut rows: Vec<Vec<Value>> = Vec::with_capacity(plan.rows.len());
        for planned in plan.rows {
            let mut values = Vec::with_capacity(planned.len());
            for cell in planned {
                match cell {
                    CopyCell::Ready(value) => values.push(value),
                    CopyCell::Fetch { locator, column } => {
                        match state
                            .load_cell(id, table.clone(), locator, column.clone())
                            .await
                        {
                            // The value grew past the cap between the page
                            // fetch and now, or the page's length estimate was
                            // low: refuse rather than copy the prefix.
                            Ok(fetch) if fetch.capped => {
                                status.set(Some(CopyStatus::failed(
                                    CopyRefusal::TooLarge {
                                        column,
                                        full_len: fetch.full_len,
                                    }
                                    .message(),
                                )));
                                return;
                            }
                            Ok(fetch) => values.push(fetch.value),
                            Err(err) => {
                                status.set(Some(CopyStatus::failed(format!("Copy failed: {err}"))));
                                return;
                            }
                        }
                    }
                }
            }
            rows.push(values);
        }
        // Report what actually landed on the clipboard, not the selection's
        // shape: `plan_copy` skips cells outside the page, so in the (clamp-
        // protected) race where the page shrank underneath the selection these
        // can differ.
        let copied_rows = rows.len();
        let copied_cols = plan.columns.len();
        let text = if raw {
            rows.first()
                .and_then(|row| row.first())
                .map(raw_cell_text)
                .unwrap_or_default()
        } else {
            let block = CopyBlock {
                schema: table.schema.clone(),
                table: table.name.clone(),
                columns: plan.columns,
                rows,
            };
            match render_copy(&block, format, dialect) {
                Some(text) => text,
                // Only reachable for INSERT with no dialect, which the caller
                // already gates on — belt and braces rather than a guess.
                None => {
                    status.set(Some(CopyStatus::failed(
                        CopyRefusal::UnknownDialect.message(),
                    )));
                    return;
                }
            }
        };
        write_clipboard(&text);
        status.set(Some(CopyStatus::ok(copy_summary(
            raw,
            format,
            copied_rows,
            copied_cols,
        ))));
    });
}

/// The success line for a finished copy.
fn copy_summary(raw: bool, format: CopyFormat, rows: usize, cols: usize) -> String {
    if raw {
        return "Copied the cell value".to_string();
    }
    if rows == 1 && cols == 1 {
        format!("Copied 1 cell as {}", format.label())
    } else {
        format!("Copied {rows}×{cols} cells as {}", format.label())
    }
}

/// Puts `text` on the system clipboard through the webview.
///
/// `navigator.clipboard` is the modern path; the hidden-textarea
/// `execCommand` fallback covers a webview that withholds it (no secure
/// context, or a rejected permission), because a copy that silently does
/// nothing is indistinguishable from a broken app. The fallback restores the
/// previously focused element so the grid keeps its keyboard focus.
fn write_clipboard(text: &str) {
    let json = serde_json::to_string(text).unwrap_or_else(|_| "\"\"".into());
    document::eval(&format!(
        r#"(() => {{
  const text = {json};
  const fallback = () => {{
    const prev = document.activeElement;
    const ta = document.createElement('textarea');
    ta.value = text;
    ta.style.position = 'fixed';
    ta.style.top = '-1000px';
    document.body.appendChild(ta);
    ta.select();
    try {{ document.execCommand('copy'); }} catch (e) {{ /* nothing else to try */ }}
    document.body.removeChild(ta);
    if (prev && prev.focus) prev.focus();
  }};
  if (navigator.clipboard && navigator.clipboard.writeText) {{
    navigator.clipboard.writeText(text).catch(fallback);
  }} else {{
    fallback();
  }}
}})();"#
    ));
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
    /// Shift-click: select this whole column instead of sorting (FRE-110).
    on_select_column: EventHandler<()>,
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
                title: "Click to sort, Shift+click to select the column",
                onclick: move |evt: MouseEvent| {
                    if evt.modifiers().shift() {
                        on_select_column.call(());
                    } else {
                        on_sort.call(clicked_name.clone());
                    }
                },
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
    /// The table's introspected render metadata, shared by pointer with every
    /// other row rather than deep-cloned into each of them (FRE-130).
    meta: Shared<TableRenderMeta>,
    dialect: Dialect,
    editing: Signal<Option<ActiveEdit>>,
    /// The keyboard-focused column in this row (FRE-15), or `None` when the
    /// focus ring is on another row.
    focused_col: Option<usize>,
    /// This row's index on the page, so a clicked cell can address itself.
    row_index: usize,
    /// The inclusive span of selected columns in this row (FRE-110), or
    /// `None` when the selection rectangle doesn't cover this row.
    selected_cols: Option<(usize, usize)>,
    /// A click on a cell: `(row, column, shift held)`.
    on_select_cell: EventHandler<(usize, usize, bool)>,
    select_enabled: bool,
    mut selected: Signal<HashMap<String, RowLocator>>,
    /// Follows the FK a clicked cell belongs to, carrying that FK plus this
    /// row's column → value map (the source of the jump's equality filter).
    on_fk_jump: EventHandler<(ForeignKeyMeta, HashMap<String, Value>)>,
) -> Element {
    // Every cell resolved against the table metadata exactly once (FRE-130):
    // the editor kind and nullability, whether the cell may open an editor,
    // and the foreign key it can jump through. The slot below used to redo
    // three hash lookups per cell for the same three answers.
    let cells: Vec<RowCell> = row
        .cells
        .iter()
        .map(|cell| {
            let (kind, nullable) = meta.kind_of(&cell.column);
            RowCell {
                editable: editable_for_kind(cell, &kind),
                fk: meta.fk_of(&cell.column, &cell.value).cloned(),
                cell: cell.clone(),
                kind,
                nullable,
            }
        })
        .collect();
    // This row's Tab order: its editable columns, left to right.
    let editable_columns = Shared::new(
        cells
            .iter()
            .filter(|resolved| resolved.editable)
            .map(|resolved| resolved.cell.column.clone())
            .collect::<Vec<String>>(),
    );
    // The row's values by column, the source for any FK jump from this row.
    // Shared, so the cells carry a pointer to it rather than a copy each.
    let row_values = Shared::new(
        row.cells
            .iter()
            .map(|cell| (cell.column.clone(), cell.value.clone()))
            .collect::<HashMap<String, Value>>(),
    );
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
            for (col_index , resolved) in cells.into_iter().enumerate() {
                GridCellSlot {
                    key: "{resolved.cell.column}",
                    id,
                    table: table.clone(),
                    row_key: row.key.clone(),
                    locator: row.locator.clone(),
                    kind: resolved.kind,
                    nullable: resolved.nullable,
                    editable: resolved.editable,
                    focused: focused_col == Some(col_index),
                    selected: selected_cols
                        .is_some_and(|(left, right)| (left..=right).contains(&col_index)),
                    row_index,
                    col_index,
                    on_select_cell,
                    dialect,
                    editable_columns: editable_columns.clone(),
                    editing,
                    // FK cells (non-NULL value belonging to an FK) carry the
                    // jump payload: the FK plus this row's values. A NULL FK
                    // references nothing, so it renders as a plain cell.
                    fk_jump: resolved.fk.map(|fk| (fk, row_values.clone())),
                    cell: resolved.cell,
                    on_fk_jump,
                }
            }
        }
    }
}

/// One cell of a rendered row, resolved against [`TableRenderMeta`] once
/// (FRE-130) instead of looked up again for each of the three answers a
/// [`GridCellSlot`] needs.
struct RowCell {
    cell: CellView,
    kind: EditorKind,
    nullable: bool,
    /// Row-level editability narrowed by the column's type — the same answer
    /// [`cell_editable`] gives.
    editable: bool,
    /// The foreign key this cell can be followed through, if any.
    fk: Option<ForeignKeyMeta>,
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
    /// Whether this cell is inside the selection rectangle (FRE-110).
    selected: bool,
    /// This cell's page coordinates, for the click-to-select handler.
    row_index: usize,
    col_index: usize,
    on_select_cell: EventHandler<(usize, usize, bool)>,
    dialect: Dialect,
    editable_columns: Shared<Vec<String>>,
    mut editing: Signal<Option<ActiveEdit>>,
    /// `Some((fk, row_values))` when this cell belongs to a foreign key and
    /// has a non-NULL value — renders a ↗ jump link (FRE-29). Editing the
    /// cell value stays on double-click/Enter, so navigation and editing never
    /// contend for the same gesture.
    fk_jump: Option<(ForeignKeyMeta, Shared<HashMap<String, Value>>)>,
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
        let draft = editing
            .read()
            .as_ref()
            .and_then(|active| active.draft.clone());
        let draft_row_key = row_key.clone();
        let draft_column = column.clone();
        rsx! {
            CellEditor {
                kind,
                dialect,
                nullable,
                initial: cell.value.clone(),
                draft,
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
                        draft: None,
                    }));
                },
                on_cancel: move |_| editing.set(None),
                on_draft: move |text: String| {
                    // Stash only while this cell is still the active edit —
                    // a deliberate switch to another cell or a sort/filter
                    // reset must not be hijacked back to the invalid editor.
                    let still_active = editing.peek().as_ref().is_some_and(|active| {
                        active.row_key == draft_row_key && active.column == draft_column
                    });
                    if still_active {
                        editing.set(Some(ActiveEdit {
                            row_key: draft_row_key.clone(),
                            column: draft_column.clone(),
                            draft: Some(text),
                        }));
                    }
                },
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
                    draft: None,
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
        // One background per cell — Tailwind classes can't be layered, so the
        // four combinations of dirty × selected (FRE-110) are spelled out. A
        // dirty cell keeps its amber, deepened while selected, so staged edits
        // never disappear under the selection tint.
        let background = match (cell.dirty, selected) {
            (true, true) => " bg-amber-200 dark:bg-amber-800/60",
            (true, false) => " bg-amber-100 dark:bg-amber-900/40",
            (false, true) => " bg-sky-100 dark:bg-sky-900/40",
            (false, false) => "",
        };
        let class = format!("px-3 py-1 {text}{background}{ring}");
        rsx! {
            td {
                class,
                id: if focused { "dv-focused-cell" },
                // A click moves the focus ring here; Shift extends the
                // selection to it (FRE-110).
                onclick: move |evt: MouseEvent| {
                    on_select_cell.call((row_index, col_index, evt.modifiers().shift()));
                },
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
                                on_fk_jump.call((fk.clone(), (*row_values).clone()));
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
    /// The visible columns, shared with the grid's header row (FRE-130).
    headers: Shared<Vec<String>>,
    /// The table's introspected render metadata — the editor kinds and the
    /// required-column set, shared by pointer with every other row.
    meta: Shared<TableRenderMeta>,
    dialect: Dialect,
    /// Whether the grid renders the leading checkbox column (it does
    /// whenever inserts are possible; this keeps the phantom row aligned).
    lead_cell: bool,
    editing: Signal<Option<ActiveEdit>>,
) -> Element {
    let state = use_context::<AppState>();
    let insert_id = insert.id();
    let row_key = insert.row_key();
    // Each column resolved against the metadata once, like a fetched row's
    // cells (FRE-130).
    let columns: Vec<(String, EditorKind, bool)> = headers
        .iter()
        .map(|column| {
            let (kind, nullable) = meta.kind_of(column);
            (column.clone(), kind, nullable)
        })
        .collect();
    // Tab order: every editable column (blob and generated cells stay
    // "default" — there is no blob editor, and generated columns are
    // database-assigned). Columns missing from the metadata edit as text,
    // same fallback as existing rows.
    let editable_columns = Shared::new(
        columns
            .iter()
            .filter(|(_, kind, _)| !kind.is_read_only())
            .map(|(column, _, _)| column.clone())
            .collect::<Vec<String>>(),
    );
    let remove_table = table.clone();
    rsx! {
        tr { class: "border-t border-dashed border-emerald-300 dark:border-emerald-700/60 bg-emerald-100 dark:bg-emerald-950/40",
            if lead_cell {
                td { class: "w-8 px-2 py-1",
                    button {
                        class: "rounded px-1.5 text-xs text-emerald-700 dark:text-emerald-300/80 hover:bg-red-100 dark:hover:bg-red-900/40 hover:text-red-600 dark:hover:text-red-300",
                        title: "Remove this pending insert (stages nothing)",
                        onclick: move |_| state.remove_pending_insert(id, &remove_table, insert_id),
                        X { size: 12 }
                    }
                }
            }
            for (column , kind , nullable) in columns {
                InsertCellSlot {
                    key: "{column}",
                    id,
                    table: table.clone(),
                    insert_id,
                    row_key: row_key.clone(),
                    override_value: insert.value(&column).cloned(),
                    missing: meta.required.contains(&column) && insert.lacks_value(&column),
                    kind,
                    nullable,
                    dialect,
                    editable_columns: editable_columns.clone(),
                    editing,
                    column,
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
    editable_columns: Shared<Vec<String>>,
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
        let draft = editing
            .read()
            .as_ref()
            .and_then(|active| active.draft.clone());
        let draft_row_key = row_key.clone();
        let draft_column = column.clone();
        rsx! {
            CellEditor {
                kind,
                dialect,
                nullable,
                initial: override_value.clone().unwrap_or(Value::Null),
                draft,
                on_draft: move |text: String| {
                    // Stash only while this cell is still the active edit —
                    // a deliberate switch to another cell or a sort/filter
                    // reset must not be hijacked back to the invalid editor.
                    let still_active = editing.peek().as_ref().is_some_and(|active| {
                        active.row_key == draft_row_key && active.column == draft_column
                    });
                    if still_active {
                        editing.set(Some(ActiveEdit {
                            row_key: draft_row_key.clone(),
                            column: draft_column.clone(),
                            draft: Some(text),
                        }));
                    }
                },
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
                        draft: None,
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
                draft: None,
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
    editable_columns: Shared<Vec<String>>,
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
            let draft = editing
                .read()
                .as_ref()
                .and_then(|active| active.draft.clone());
            let draft_row_key = row_key.clone();
            let draft_column = column.clone();
            rsx! {
                CellEditor {
                    kind,
                    dialect,
                    nullable,
                    initial,
                    draft,
                    on_draft: move |text: String| {
                        // Stash only while this cell is still the active
                        // edit — a deliberate switch to another cell or a
                        // sort/filter reset must not be hijacked back to
                        // the invalid editor.
                        let still_active = editing.peek().as_ref().is_some_and(|active| {
                            active.row_key == draft_row_key && active.column == draft_column
                        });
                        if still_active {
                            editing.set(Some(ActiveEdit {
                                row_key: draft_row_key.clone(),
                                column: draft_column.clone(),
                                draft: Some(text),
                            }));
                        }
                    },
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
                            draft: None,
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
            // A capped fetch holds a prefix, so Copy raw is withdrawn rather
            // than handed a truncated value — the same refusal `plan_copy`
            // makes for this cell, worded identically (FRE-110).
            let capped_note = capped.then(|| {
                format!(
                    "Value is very large; showing the first {}. {}",
                    human_bytes(FETCH_CELL_MAX_BYTES as u64),
                    CopyRefusal::TooLarge {
                        column: column.clone(),
                        full_len: fetch.full_len,
                    }
                    .message(),
                )
            });
            rsx! {
                if let Some(note) = capped_note {
                    Banner { kind: BannerKind::Warning, message: note }
                } else {
                    CopyRawButton { raw: text.clone() }
                }
                pre { class: "mt-2 whitespace-pre-wrap break-words font-mono text-xs text-slate-900 dark:text-slate-200",
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
    rsx! {
        div { class: "mb-1 flex justify-end",
            button {
                class: "rounded border border-slate-300 dark:border-slate-700 px-1.5 py-0.5 text-xs text-slate-500 dark:text-slate-400 hover:bg-slate-200 dark:hover:bg-slate-800 hover:text-slate-900 dark:hover:text-slate-100",
                title: "Copy the raw value (not the formatted view)",
                onclick: move |_| write_clipboard(&raw),
                "Copy raw"
            }
        }
    }
}

/// The row detail panel's drag-to-resize listener (FRE-109). Same shape as
/// [`GRID_SCROLL_JS`]: the drag is handled entirely in JS, which moves the
/// node itself and reports the resting width back once, on release — so a
/// drag costs no re-renders and the width still ends up in Rust, where it
/// survives a table switch. The move/up listeners are added on pointerdown
/// and removed on pointerup, so closing the panel leaves nothing behind.
fn detail_resize_js() -> String {
    format!(
        r#"
(() => {{
  const panel = document.getElementById('dv-row-detail');
  const handle = document.getElementById('dv-row-detail-handle');
  if (!panel || !handle) return;
  const min = {DETAIL_MIN_WIDTH}, max = {DETAIL_MAX_WIDTH};
  let startX = 0, startWidth = 0;
  const onMove = (e) => {{
    // The panel is docked to the right edge, so dragging left widens it.
    const width = Math.min(max, Math.max(min, startWidth + (startX - e.clientX)));
    panel.style.width = width + 'px';
  }};
  const onUp = () => {{
    window.removeEventListener('pointermove', onMove);
    window.removeEventListener('pointerup', onUp);
    document.body.style.userSelect = '';
    dioxus.send(panel.getBoundingClientRect().width);
  }};
  handle.addEventListener('pointerdown', (e) => {{
    e.preventDefault();
    startX = e.clientX;
    startWidth = panel.getBoundingClientRect().width;
    // Suppressed for the duration of the drag only, so a drag across the
    // grid doesn't paint a text selection behind it.
    document.body.style.userSelect = 'none';
    window.addEventListener('pointermove', onMove);
    window.addEventListener('pointerup', onUp);
  }});
}})();
"#
    )
}

/// The row detail panel (FRE-109): the focused row as a vertical
/// column → value form, docked to the right of the rows.
///
/// It is a companion to browsing rather than a mode: it stays open while the
/// focus moves, so the grid keeps its context. Its Prev/Next move the
/// **grid's** focus (through the same [`apply_grid_move`] an arrow key takes)
/// instead of tracking a row of its own, and everything it renders — values,
/// staged tints, editability — is read off the [`GridNav`] snapshot the grid
/// renders from. Edits stage through [`AppState::stage_cell_edit`], so they
/// land in the same set, and save through the same button, as a grid edit.
#[component]
fn RowDetailPanel(
    id: ConnectionId,
    table: TableRef,
    /// The focused row, or `None` when the page has no rows. [`Shared`] so
    /// this prop stays pointer-comparable while the grid re-renders around it
    /// (FRE-130) — the panel only rebuilds when the focused row changes.
    detail: Option<Shared<RowDetail>>,
    width: f64,
    dialect: Dialect,
    /// Why editing is unavailable (FRE-87/FRE-111) — the grid's sentence,
    /// from the grid's resolution, stated once instead of per field.
    read_only_notice: Option<String>,
    /// The grid's in-place editor, closed when a field here opens one: only
    /// one cell editor can hold the keyboard at a time.
    grid_editing: Signal<Option<ActiveEdit>>,
    /// The panel's open editor, owned above this component so it survives the
    /// keyed remount of `RowDetailFields` (FRE-109).
    editing: Signal<Option<ActiveEdit>>,
    on_step: EventHandler<RowStep>,
    on_close: EventHandler<()>,
    on_width: EventHandler<f64>,
    on_fk_jump: EventHandler<(ForeignKeyMeta, HashMap<String, Value>)>,
) -> Element {
    // Install the resize listener once per mount; it reads no signals, so it
    // never re-installs, and the eval channel dies with the panel.
    use_effect(move || {
        spawn(async move {
            let mut channel = document::eval(&detail_resize_js());
            while let Ok(dragged) = channel.recv::<f64>().await {
                on_width.call(clamp_detail_width(dragged));
            }
        });
    });

    let position = detail.as_ref().map(|detail| detail.position);
    let step_class = "rounded px-1.5 py-0.5 text-slate-500 dark:text-slate-400 \
                      hover:bg-slate-200 dark:hover:bg-slate-800 hover:text-slate-900 \
                      dark:hover:text-slate-100 disabled:opacity-30";
    rsx! {
        aside {
            id: "dv-row-detail",
            class: "relative flex shrink-0 flex-col border-l border-slate-200 dark:border-slate-800 bg-slate-50 dark:bg-slate-950/40",
            style: "width:{width}px;",
            // The drag handle rides the docked edge; the content is padded
            // clear of it.
            div {
                id: "dv-row-detail-handle",
                class: "absolute inset-y-0 left-0 z-10 w-1.5 cursor-col-resize hover:bg-sky-400/50",
                title: "Drag to resize",
            }
            div { class: "flex items-center gap-1 border-b border-slate-200 dark:border-slate-800 py-1.5 pl-3 pr-2 text-xs",
                span { class: "font-semibold text-slate-700 dark:text-slate-300",
                    match position {
                        Some(position) => format!("Row {} of {}", position.number, position.total),
                        None => "Row detail".to_string(),
                    }
                }
                div { class: "flex-1" }
                button {
                    class: step_class,
                    title: "Previous row (or ↑ in the grid)",
                    disabled: !position.is_some_and(|position| position.has_prev()),
                    onclick: move |_| on_step.call(RowStep::Prev),
                    ChevronUp { size: 14 }
                }
                button {
                    class: step_class,
                    title: "Next row (or ↓ in the grid)",
                    disabled: !position.is_some_and(|position| position.has_next()),
                    onclick: move |_| on_step.call(RowStep::Next),
                    ChevronDown { size: 14 }
                }
                button {
                    class: step_class,
                    aria_label: "Close the row detail panel",
                    title: "Close (Ctrl+D)",
                    onclick: move |_| on_close.call(()),
                    X { size: 14 }
                }
            }
            div { class: "min-h-0 flex-1 overflow-auto",
                if let Some(notice) = read_only_notice {
                    div { class: "p-2",
                        Banner { kind: BannerKind::Info, message: notice }
                    }
                }
                match detail {
                    Some(detail) => rsx! {
                        RowDetailFields {
                            // Keyed by row: every field owns a fetch for its
                            // full value, so a move to another row has to
                            // remount them rather than show the last one's.
                            key: "{detail.row_key}",
                            id,
                            table,
                            dialect,
                            fields: detail.fields.clone(),
                            locator: detail.locator.clone(),
                            row_values: detail.row_values.clone(),
                            row_key: detail.row_key.clone(),
                            grid_editing,
                            editing,
                            on_fk_jump,
                        }
                    },
                    None => rsx! {
                        p { class: "p-3 text-xs text-slate-500 dark:text-slate-400",
                            "No row to show — this page is empty."
                        }
                    },
                }
            }
        }
    }
}

/// The focused row's fields. Mounted keyed by row (see [`RowDetailPanel`]),
/// so the open editor and the per-field fetches reset when the focus moves.
#[component]
fn RowDetailFields(
    id: ConnectionId,
    table: TableRef,
    fields: Vec<DetailField>,
    locator: Option<RowLocator>,
    row_values: Shared<HashMap<String, Value>>,
    dialect: Dialect,
    grid_editing: Signal<Option<ActiveEdit>>,
    /// The open editor, owned by `DataGrid` rather than created here. This
    /// component is keyed by row, so a signal created here would be destroyed
    /// by every row move — taking the user's uncommitted text with it.
    /// The focused row's identity, so an open editor is matched on row as well
    /// as column and cannot reappear on a same-named field of another row.
    row_key: String,
    editing: Signal<Option<ActiveEdit>>,
    on_fk_jump: EventHandler<(ForeignKeyMeta, HashMap<String, Value>)>,
) -> Element {
    // Tab order: the editable fields, top to bottom.
    //
    // A field whose full value exceeds the fetch cap renders a note instead of
    // an editor, so including it would dead-end the Tab walk on something that
    // can never take focus. `PreviewInfo::full_len` predicts that before the
    // fetch resolves, which is the same answer `CellFetch::capped` gives after.
    //
    // Shared, so each field carries a pointer to the list rather than its own
    // copy of it (FRE-130).
    let editable_columns = Shared::new(
        fields
            .iter()
            .filter(|field| field.editable && !field.over_fetch_cap())
            .map(|field| field.column.clone())
            .collect::<Vec<String>>(),
    );
    rsx! {
        dl { class: "divide-y divide-slate-200 dark:divide-slate-800",
            for field in fields {
                RowDetailRow {
                    key: "{field.column}",
                    id,
                    table: table.clone(),
                    dialect,
                    field,
                    locator: locator.clone(),
                    row_values: row_values.clone(),
                    editable_columns: editable_columns.clone(),
                    row_key: row_key.clone(),
                    editing,
                    grid_editing,
                    on_fk_jump,
                }
            }
        }
    }
}

/// One column of the focused row: its name and type, an FK jump when it has
/// one, and its value — the full value, not the grid's preview (FRE-109).
#[component]
fn RowDetailRow(
    id: ConnectionId,
    table: TableRef,
    field: DetailField,
    locator: Option<RowLocator>,
    row_values: Shared<HashMap<String, Value>>,
    dialect: Dialect,
    editable_columns: Shared<Vec<String>>,
    /// The focused row's identity, so an open editor is matched on row as well
    /// as column and cannot reappear on a same-named field of another row.
    row_key: String,
    editing: Signal<Option<ActiveEdit>>,
    grid_editing: Signal<Option<ActiveEdit>>,
    on_fk_jump: EventHandler<(ForeignKeyMeta, HashMap<String, Value>)>,
) -> Element {
    rsx! {
        div {
            // Staged fields carry the grid's amber, so a change made in
            // either place reads the same in both.
            class: if field.dirty { "px-3 py-2 bg-amber-100 dark:bg-amber-900/25" } else { "px-3 py-2" },
            dt { class: "mb-1 flex items-baseline gap-2",
                span { class: "min-w-0 break-all font-mono text-xs font-semibold text-slate-900 dark:text-slate-200",
                    "{field.column}"
                }
                span { class: "shrink-0 font-mono text-[11px] text-slate-500 dark:text-slate-400",
                    "{field.type_name}"
                }
                div { class: "flex-1" }
                if field.dirty {
                    span {
                        class: "shrink-0 rounded bg-amber-200 dark:bg-amber-800/60 px-1 text-[10px] leading-tight text-amber-800 dark:text-amber-200",
                        title: "Staged, not yet saved",
                        "edited"
                    }
                }
                // Same jump the grid's ↗ makes, from the same row values.
                if let Some(fk) = field.fk.clone() {
                    a {
                        class: "shrink-0 cursor-pointer select-none text-cyan-600 dark:text-cyan-400 hover:underline",
                        title: "Go to {fk.referenced_table}",
                        onclick: move |_| on_fk_jump.call((fk.clone(), (*row_values).clone())),
                        "↗"
                    }
                }
            }
            dd {
                match (&field.preview, &locator) {
                    // A truncated cell whose row can be addressed: load the
                    // whole value through the shared cell-fetch path (FRE-33).
                    (Some(_), Some(locator)) => rsx! {
                        RowDetailFullValue {
                            id,
                            table,
                            dialect,
                            field: field.clone(),
                            locator: locator.clone(),
                            editable_columns,
                            row_key: row_key.clone(),
                            editing,
                            grid_editing,
                        }
                    },
                    // Truncated and unaddressable (a view, a keyless table):
                    // the preview is all there will ever be, and the same
                    // refusal the copy path states says why.
                    (Some(preview), None) => {
                        // A blob renders as `<blob N>`, and `N` derived from
                        // the value in hand would be the *prefix's* size. The
                        // page already reports the real length, so read it
                        // rather than recompute it from something known to be
                        // truncated — the re-derive trap the SQL Server length
                        // probe fix closed on main.
                        let shown = match &field.value {
                            Value::Blob(_) => format!("<blob {}>", human_bytes(preview.full_len)),
                            other => format!("{}…", other.display()),
                        };
                        rsx! {
                            Banner {
                                kind: BannerKind::Warning,
                                message: CopyRefusal::Unaddressable { column: field.column.clone() }.message(),
                            }
                            pre { class: "mt-1 max-h-64 overflow-auto whitespace-pre-wrap break-words font-mono text-xs text-slate-900 dark:text-slate-200",
                                "{shown}"
                            }
                        }
                    },
                    // Already complete in the page.
                    (None, _) => rsx! {
                        RowDetailValue {
                            id,
                            table,
                            dialect,
                            value: field.value.clone(),
                            field: field.clone(),
                            locator: locator.clone(),
                            editable_columns,
                            row_key: row_key.clone(),
                            editing,
                            grid_editing,
                        }
                    },
                }
            }
        }
    }
}

/// A field whose grid cell holds only a bounded preview: loads the full value
/// through [`AppState::load_cell`] — the same path the expand popup and the
/// clipboard use (FRE-33) — and then renders and edits it like any other
/// field. A value over [`FETCH_CELL_MAX_BYTES`] cannot be loaded whole, so it
/// is shown as its first chunk and never opened for editing: staging the
/// prefix would silently truncate what is stored.
#[component]
fn RowDetailFullValue(
    id: ConnectionId,
    table: TableRef,
    field: DetailField,
    locator: RowLocator,
    dialect: Dialect,
    editable_columns: Shared<Vec<String>>,
    /// The focused row's identity, so an open editor is matched on row as well
    /// as column and cannot reappear on a same-named field of another row.
    row_key: String,
    editing: Signal<Option<ActiveEdit>>,
    grid_editing: Signal<Option<ActiveEdit>>,
) -> Element {
    let state = use_context::<AppState>();
    let fetch_table = table.clone();
    let fetch_locator = locator.clone();
    let fetch_column = field.column.clone();
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
        Some(Ok(fetch)) if fetch.capped => {
            let note = format!(
                "Value is very large; showing the first {} and not offering an editor.",
                human_bytes(FETCH_CELL_MAX_BYTES as u64),
            );
            rsx! {
                Banner { kind: BannerKind::Warning, message: note }
                pre { class: "mt-1 max-h-64 overflow-auto whitespace-pre-wrap break-words font-mono text-xs text-slate-900 dark:text-slate-200",
                    "{fetch.value.display()}"
                }
            }
        }
        Some(Ok(fetch)) => rsx! {
            RowDetailValue {
                id,
                table,
                dialect,
                value: fetch.value.clone(),
                field,
                locator: Some(locator),
                editable_columns,
                row_key: row_key.clone(),
                editing,
                grid_editing,
            }
        },
    }
}

/// A field's value: rendered for reading, or the shared [`CellEditor`] while
/// this field is the panel's active edit.
///
/// Commits go through [`AppState::stage_cell_edit`] — the same call the grid
/// cell makes — so an edit here joins the same staged set and is saved by the
/// same Save button. There is deliberately no second write route.
#[component]
fn RowDetailValue(
    id: ConnectionId,
    table: TableRef,
    field: DetailField,
    /// The complete value: fetched for a previewed cell, `field.value`
    /// otherwise. The editor must never start from a preview (FRE-33).
    value: Value,
    locator: Option<RowLocator>,
    dialect: Dialect,
    editable_columns: Shared<Vec<String>>,
    /// The focused row's identity, so an open editor is matched on row as well
    /// as column and cannot reappear on a same-named field of another row.
    row_key: String,
    mut editing: Signal<Option<ActiveEdit>>,
    mut grid_editing: Signal<Option<ActiveEdit>>,
) -> Element {
    let state = use_context::<AppState>();
    // `field.editable` already folds in the resolved capability and the
    // user's marking; the locator is what makes the row addressable.
    let editable = field.editable && locator.is_some();
    // Matched on row *and* column: the open editor outlives a move to another
    // row (it is owned by `DataGrid`), and must not reappear on whichever
    // field happens to share its name over there.
    let open = editing
        .read()
        .clone()
        .filter(|open| open.is_on(&row_key, &field.column));
    let active = editable && open.is_some();

    if active {
        let locator = locator.expect("an editable field has a locator");
        let column = field.column.clone();
        let step_columns = editable_columns.clone();
        let draft = open.and_then(|open| open.draft);
        let draft_row = row_key.clone();
        let draft_column = field.column.clone();
        let step_row = row_key.clone();
        return rsx! {
            CellEditor {
                // A block wrapper, not a table cell: this is a form, not a row.
                block: true,
                kind: field.kind.clone(),
                dialect,
                nullable: field.nullable,
                initial: value,
                draft,
                on_commit: move |(committed, nav): (Option<Value>, EditNav)| {
                    if let Some(committed) = committed {
                        state.stage_cell_edit(id, &table, locator.clone(), &column, committed);
                    }
                    // Tab walks the panel's fields the way it walks a row's
                    // cells in the grid.
                    let next = match nav {
                        EditNav::Stay => None,
                        EditNav::Next => step_column(&step_columns, &column, 1),
                        EditNav::Prev => step_column(&step_columns, &column, -1),
                    };
                    editing.set(next.map(|column| ActiveEdit {
                        row_key: step_row.clone(),
                        column,
                        draft: None,
                    }));
                },
                on_cancel: move |_| editing.set(None),
                // Input that doesn't parse is stashed rather than dropped
                // (FRE-74). The grid needs this because scrolling unmounts a
                // row mid-typing; the panel needs it because `RowDetailFields`
                // is keyed by row, so *every* row move — Prev/Next, an arrow
                // key, a click, the post-save refetch — remounts every field.
                // Without it the text vanished silently, which is worse than
                // the grid, whose editor stays open showing the parse error.
                on_draft: move |text: String| {
                    // Stash only while this field is still the active edit —
                    // the same guard the grid's editor carries, and for a
                    // sharper reason here. `use_drop` fires *after* whatever
                    // closed this editor has already chosen the next one, so
                    // an unguarded stash resurrects the invalid editor and
                    // hijacks the switch: double-clicking another panel field
                    // reopened this one, and double-clicking a grid cell was
                    // swallowed entirely (the resurrected editor stole the
                    // shared element id back, blurring the grid's).
                    //
                    // That also makes this guard load-bearing for the
                    // one-editor invariant, not just for focus. Every route to
                    // the grid blurs this input first, which commits and
                    // closes it *unless* the text doesn't parse — so
                    // unparseable text is precisely the case the reverse guard
                    // in `DataGrid` has to handle, and precisely the case an
                    // unguarded `on_draft` would undo.
                    let still_active = editing
                        .peek()
                        .as_ref()
                        .is_some_and(|active| active.is_on(&draft_row, &draft_column));
                    if still_active {
                        editing.set(Some(ActiveEdit {
                            row_key: draft_row.clone(),
                            column: draft_column.clone(),
                            draft: Some(text),
                        }));
                    }
                },
            }
        };
    }

    // Captures only signals, so it is `Copy` and both affordances below can
    // take it. Closing the grid's editor first keeps a single one mounted:
    // two would share an element id and race for the keyboard. Guarded on one
    // actually being open — an unconditional `set` still marks the signal
    // dirty, and the grid's "nothing is being edited, take the focus back"
    // effect would then yank focus off the editor mounting here.
    // Takes the row and column rather than capturing them, so it stays
    // `Copy` (capturing a `String` would move it into the first closure) and
    // both affordances below can use it.
    let mut activate = move |row_key: &str, column: &str| {
        if grid_editing.peek().is_some() {
            grid_editing.set(None);
        }
        editing.set(Some(ActiveEdit {
            row_key: row_key.to_string(),
            column: column.to_string(),
            draft: None,
        }));
    };
    let dbl_row = row_key.clone();
    let button_row = row_key.clone();
    let dbl_column = field.column.clone();
    let button_column = field.column.clone();
    let display = value.display();
    rsx! {
        div { class: "flex items-start gap-1",
            div {
                class: "min-w-0 flex-1",
                ondoubleclick: move |_| {
                    if editable {
                        activate(&dbl_row, &dbl_column);
                    }
                },
                match &value {
                    // NULL reads distinctly from an empty string, exactly as
                    // it does in the grid.
                    Value::Null => rsx! {
                        span { class: "font-mono text-xs italic text-slate-400 dark:text-slate-600", "NULL" }
                    },
                    Value::Blob(_) => rsx! {
                        span { class: "font-mono text-xs text-violet-700 dark:text-violet-400", "{display}" }
                    },
                    _ => rsx! {
                        pre { class: "max-h-64 overflow-auto whitespace-pre-wrap break-words font-mono text-xs text-slate-900 dark:text-slate-200",
                            "{display}"
                        }
                    },
                }
            }
            if editable {
                button {
                    class: "shrink-0 rounded p-0.5 text-slate-400 opacity-60 hover:bg-slate-200 dark:hover:bg-slate-800 hover:opacity-100",
                    title: "Edit this value (or double-click it)",
                    onclick: move |_| activate(&button_row, &button_column),
                    Pencil { size: 12 }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_open_editor_is_matched_on_row_as_well_as_column() {
        // The detail panel's editor outlives a row move (it is owned above the
        // row-keyed field list, so a draft survives). Matching on column alone
        // would reopen it on the next row's same-named field, seeded with the
        // previous row's text.
        let open = ActiveEdit {
            row_key: "1".into(),
            column: "name".into(),
            draft: Some("half-typed".into()),
        };
        assert!(open.is_on("1", "name"));
        assert!(!open.is_on("2", "name"), "same column, different row");
        assert!(!open.is_on("1", "email"), "same row, different column");
        assert!(!open.is_on("2", "email"));
    }

    #[test]
    fn a_field_past_the_fetch_cap_is_left_out_of_the_tab_order() {
        // A capped field renders a note instead of an editor, so including it
        // would dead-end Tab on something that can never take focus.
        let field = |preview: Option<PreviewInfo>| DetailField {
            column: "body".into(),
            type_name: "text".into(),
            value: Value::Text("x".into()),
            preview,
            dirty: false,
            kind: EditorKind::Text,
            nullable: true,
            editable: true,
            fk: None,
        };
        assert!(
            !field(None).over_fetch_cap(),
            "a complete value is editable"
        );
        assert!(
            !field(Some(PreviewInfo {
                full_len: FETCH_CELL_MAX_BYTES as u64,
                binary: false,
            }))
            .over_fetch_cap(),
            "exactly at the cap still fetches whole"
        );
        assert!(
            field(Some(PreviewInfo {
                full_len: FETCH_CELL_MAX_BYTES as u64 + 1,
                binary: false,
            }))
            .over_fetch_cap(),
            "past the cap renders a note, not an editor"
        );
    }
    use crate::db::PREVIEW_BYTES;

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
        let rows = view_rows(&result, &[], 0, Some(&pk_identity()), Some(&stage), true);
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
        let rows = view_rows(&result, &[], 1, Some(&identity), Some(&stage), true);
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
        let rows = view_rows(&result, &[], 0, None, Some(&stage), true);
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
        let rows = view_rows(&result, &[], 0, Some(&pk_identity()), None, true);
        assert!(rows[0].locator.is_some());
        assert!(cell_editable(&rows[0].cells[1], &kinds));
        // …a column missing from the metadata falls back to editable text…
        let (kind, nullable) = cell_kind(&rows[0].cells[1], &HashMap::new());
        assert_eq!(kind, EditorKind::Text);
        assert!(nullable);
        // …but without an identity nothing is.
        let rows = view_rows(&result, &[], 0, None, None, true);
        assert!(rows[0].locator.is_none());
        assert!(!rows[0].cells[1].editable);

        // Without the write capability nothing is editable either, even
        // though the rows stay addressable — a read-only connection can
        // still expand a cell, which needs the locator (FRE-87).
        let rows = view_rows(&result, &[], 0, Some(&pk_identity()), None, false);
        assert!(rows[0].locator.is_some());
        assert!(rows.iter().all(|r| r.cells.iter().all(|c| !c.editable)));

        // Rows pending deletion are not editable.
        let mut stage = TableStage::default();
        stage.mark_delete(RowLocator {
            identity_values: vec![Value::Integer(1)],
        });
        let rows = view_rows(&result, &[], 0, Some(&pk_identity()), Some(&stage), true);
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
        let rows = view_rows(&blob_result, &[], 0, Some(&pk_identity()), None, true);
        assert!(!rows[0].cells[1].editable, "blob value cell");
        let null_blob = QueryResult {
            rows: vec![vec![Value::Integer(1), Value::Null]],
            ..blob_result
        };
        let rows = view_rows(&null_blob, &[], 0, Some(&pk_identity()), None, true);
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
    fn only_addressable_undeleted_rows_can_be_ticked() {
        let result = two_column_result();
        // Nothing staged: both rows are addressable, so both are selectable.
        let rows = view_rows(&result, &[], 0, Some(&pk_identity()), None, true);
        let keys: Vec<String> = selectable_rows(&rows)
            .into_iter()
            .map(|(key, _)| key)
            .collect();
        assert_eq!(keys.len(), 2);
        assert_eq!(
            selectable_rows(&rows)[0].1.identity_values,
            vec![Value::Integer(1)],
            "the locator travels with the key",
        );
        // A row already pending delete drops out — ticking it again would
        // stage a second delete for a row the user can no longer see.
        let mut stage = TableStage::default();
        stage.mark_delete(RowLocator {
            identity_values: vec![Value::Integer(1)],
        });
        let rows = view_rows(&result, &[], 0, Some(&pk_identity()), Some(&stage), true);
        assert_eq!(selectable_rows(&rows).len(), 1);
        assert_eq!(selectable_rows(&rows)[0].0, keys[1]);
        // A table with no identity has nothing to address, so nothing to tick.
        let rows = view_rows(&result, &[], 0, None, None, true);
        assert!(selectable_rows(&rows).is_empty());
    }

    #[test]
    fn a_windowed_row_keeps_its_index_on_the_whole_page() {
        // The focus ring, the selection rectangle and the click handler all
        // address rows by page index, so slicing the window must not renumber
        // them from zero.
        let result = QueryResult {
            rows: (1..=5)
                .map(|n| vec![Value::Integer(n), Value::Text(format!("row {n}"))])
                .collect(),
            ..two_column_result()
        };
        let rows = view_rows(&result, &[], 0, Some(&pk_identity()), None, true);
        let window = window_rows(&rows, 2, 4);
        assert_eq!(
            window.iter().map(|(index, _)| *index).collect::<Vec<_>>(),
            [2, 3]
        );
        assert_eq!(window[0].1.cells[1].value, Value::Text("row 3".into()));
        // The whole page and an empty window are both fine.
        assert_eq!(window_rows(&rows, 0, 5).len(), 5);
        assert!(window_rows(&rows, 5, 5).is_empty());
    }

    #[test]
    fn shared_render_data_compares_by_pointer_then_by_value() {
        // The pointer check is a fast path: two clones of one value are equal
        // without touching the contents…
        let first = Shared::new(vec!["a".to_string(), "b".to_string()]);
        let same = first.clone();
        assert_eq!(first, same);
        // …but a rebuild that produced an identical value must still compare
        // equal, or every memo rebuild would re-render every row that holds it.
        let rebuilt = Shared::new(vec!["a".to_string(), "b".to_string()]);
        assert_eq!(first, rebuilt);
        assert!(
            !Arc::ptr_eq(&first.0, &rebuilt.0),
            "…and that is genuinely a different allocation"
        );
        // Different contents still differ, so real changes propagate.
        assert_ne!(first, Shared::new(vec!["a".to_string()]));
    }

    #[test]
    fn render_metadata_resolves_kinds_and_foreign_keys_once_per_table() {
        let meta = TableRenderMeta::build(Some(&detail_table_meta()), Some(Dialect::Sqlite));
        assert_eq!(meta.schema_columns, ["id", "title"]);
        // Editor kinds come from the same helper the grid has always used.
        assert_eq!(meta.kind_of("title"), (EditorKind::Text, true));
        // A column missing from the metadata edits as nullable text.
        assert_eq!(meta.kind_of("nope"), (EditorKind::Text, true));
        // A non-NULL FK column offers the jump; the same column NULL doesn't
        // (a NULL foreign key references nothing), nor does a plain column.
        assert_eq!(
            meta.fk_of("title", &Value::Text("x".into()))
                .map(|fk| fk.referenced_table.as_str()),
            Some("titles")
        );
        assert!(meta.fk_of("title", &Value::Null).is_none());
        assert!(meta.fk_of("id", &Value::Integer(1)).is_none());
        // Without a schema (or without a connection to name the dialect)
        // nothing is resolved rather than half-resolved.
        assert_eq!(
            TableRenderMeta::build(None, None),
            TableRenderMeta::default()
        );
        assert!(TableRenderMeta::build(Some(&detail_table_meta()), None)
            .required
            .is_empty());
    }

    #[test]
    fn a_column_in_several_foreign_keys_takes_the_first() {
        // Documented v1 limit: the jump affordance follows one FK per column.
        let mut table = detail_table_meta();
        table.foreign_keys.push(ForeignKeyMeta {
            columns: vec!["title".into()],
            referenced_schema: None,
            referenced_table: "other".into(),
            referenced_columns: vec![Some("name".into())],
        });
        let meta = TableRenderMeta::build(Some(&table), Some(Dialect::Sqlite));
        assert_eq!(meta.col_to_fk["title"], 0);
        assert_eq!(
            meta.fk_of("title", &Value::Text("x".into()))
                .map(|fk| fk.referenced_table.as_str()),
            Some("titles")
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
        assert!(!save_disabled(false, 0, true));
        assert!(save_disabled(true, 0, true), "in-flight save");
        assert!(save_disabled(false, 2, true), "missing required cells");
        assert!(save_disabled(false, 0, false), "table can't be written");
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
        let rows = view_rows(&result, &[], 0, Some(&pk_identity()), None, true);
        let nav = GridNav::build(vec!["id".into(), "title".into()], &rows, &kinds);
        assert_eq!(nav.dims(), (2, 2));
        // Both cells of an identified table are editable text here.
        assert!(nav.rows[0].cells[1].editable);
        assert_eq!(nav.rows[0].cells[1].column, "title");
        assert_eq!(nav.rows[0].cells[0].display, "1");
        // Without an identity, nothing is editable and rows have no key.
        let rows = view_rows(&result, &[], 0, None, None, true);
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

    /// A one-row page whose `body` cell is a truncated preview of `full_len`
    /// (FRE-110 copy planning); `identity` decides whether its row can be
    /// addressed to load the full value.
    fn previewed_nav(full_len: u64, identity: Option<&RowIdentity>) -> GridNav {
        let result = QueryResult {
            columns: vec![
                crate::db::ColumnInfo { name: "id".into() },
                crate::db::ColumnInfo {
                    name: "body".into(),
                },
            ],
            rows: vec![vec![Value::Integer(1), Value::Text("prefix".into())]],
        };
        let previews = vec![vec![
            None,
            Some(PreviewInfo {
                full_len,
                binary: false,
            }),
        ]];
        let rows = view_rows(&result, &previews, 0, identity, None, true);
        GridNav::build(vec!["id".into(), "body".into()], &rows, &HashMap::new())
    }

    #[test]
    fn expanding_an_in_hand_cell_keeps_the_value_so_copy_raw_matches_a_grid_copy() {
        // A small blob is not truncated, so Enter expands it from the page.
        // The popup must still copy what Ctrl+C over that cell copies — the
        // hex, never the `<blob 2 B>` placeholder it displays.
        let result = QueryResult {
            columns: vec![
                crate::db::ColumnInfo { name: "id".into() },
                crate::db::ColumnInfo {
                    name: "cover".into(),
                },
            ],
            rows: vec![vec![Value::Null, Value::Blob(vec![0xde, 0xad])]],
        };
        let rows = view_rows(&result, &[], 0, Some(&pk_identity()), None, true);
        let nav = GridNav::build(vec!["id".into(), "cover".into()], &rows, &HashMap::new());
        let blob = &nav.rows[0].cells[1];
        assert_eq!(blob.display, "<blob 2 B>", "the grid shows a placeholder");
        let ExpandView::Text(value) = expand_view(blob, nav.rows[0].locator.clone()) else {
            panic!("a complete value expands in hand");
        };
        assert_eq!(raw_cell_text(&value), "\\xdead");
        // Same for NULL: the popup reads "NULL", a copy yields nothing —
        // exactly as `plan_copy` + `raw_cell_text` do for the same cell.
        let ExpandView::Text(value) = expand_view(&nav.rows[0].cells[0], None) else {
            panic!("a complete value expands in hand");
        };
        assert_eq!(value.display(), "NULL");
        assert_eq!(raw_cell_text(&value), "");
    }

    #[test]
    fn expanding_a_truncated_cell_fetches_it_or_refuses_to_copy_it() {
        let nav = previewed_nav(PREVIEW_BYTES as u64 * 4, Some(&pk_identity()));
        assert_eq!(
            expand_view(&nav.rows[0].cells[1], nav.rows[0].locator.clone()),
            ExpandView::Fetch {
                locator: RowLocator {
                    identity_values: vec![Value::Integer(1)],
                },
                column: "body".into(),
            }
        );
        // Unaddressable: the preview is readable but not copyable.
        assert_eq!(
            expand_view(&nav.rows[0].cells[1], None),
            ExpandView::Truncated {
                display: "prefix…".into(),
                column: "body".into(),
            }
        );
    }

    #[test]
    fn copy_plan_covers_exactly_the_selected_rectangle() {
        let result = two_column_result();
        let rows = view_rows(&result, &[], 0, Some(&pk_identity()), None, true);
        let nav = GridNav::build(vec!["id".into(), "title".into()], &rows, &HashMap::new());

        // One cell: one column, one value.
        let plan = plan_copy(&nav, Selection::single((1, 1))).unwrap();
        assert_eq!(plan.columns, ["title"]);
        assert_eq!(
            plan.rows,
            vec![vec![CopyCell::Ready(Value::Text("two".into()))]]
        );

        // The whole page, in row-major order.
        let plan = plan_copy(&nav, Selection::all(2, 2).unwrap()).unwrap();
        assert_eq!(plan.columns, ["id", "title"]);
        assert_eq!(
            plan.rows,
            vec![
                vec![
                    CopyCell::Ready(Value::Integer(1)),
                    CopyCell::Ready(Value::Text("one".into())),
                ],
                vec![
                    CopyCell::Ready(Value::Integer(2)),
                    CopyCell::Ready(Value::Text("two".into())),
                ],
            ]
        );

        // A whole column: both rows, one column.
        let plan = plan_copy(&nav, Selection::column(0, 2).unwrap()).unwrap();
        assert_eq!(plan.columns, ["id"]);
        assert_eq!(plan.rows.len(), 2);
        assert_eq!(plan.rows[1], vec![CopyCell::Ready(Value::Integer(2))]);
    }

    #[test]
    fn copy_plan_tickets_previewed_cells_for_a_fetch() {
        // A truncated cell must never be copied from the page: the plan asks
        // for the full value through the row's locator (FRE-110).
        let nav = previewed_nav(PREVIEW_BYTES as u64 * 4, Some(&pk_identity()));
        let plan = plan_copy(&nav, Selection::all(1, 2).unwrap()).unwrap();
        assert_eq!(
            plan.rows[0],
            vec![
                CopyCell::Ready(Value::Integer(1)),
                CopyCell::Fetch {
                    locator: RowLocator {
                        identity_values: vec![Value::Integer(1)],
                    },
                    column: "body".into(),
                },
            ]
        );
    }

    #[test]
    fn copy_plan_refuses_a_cell_over_the_fetch_cap() {
        let full_len = FETCH_CELL_MAX_BYTES as u64 + 1;
        let nav = previewed_nav(full_len, Some(&pk_identity()));
        // The oversize column is only refused when it is actually selected.
        assert!(plan_copy(&nav, Selection::single((0, 0))).is_ok());
        assert_eq!(
            plan_copy(&nav, Selection::single((0, 1))),
            Err(CopyRefusal::TooLarge {
                column: "body".into(),
                full_len,
            })
        );
        // The refusal names the column and the cap, and points at Export.
        let message = CopyRefusal::TooLarge {
            column: "body".into(),
            full_len,
        }
        .message();
        assert!(message.contains("\"body\""), "{message}");
        assert!(message.contains("8.0 MB"), "{message}");
        assert!(message.contains("Export"), "{message}");
    }

    #[test]
    fn copy_plan_refuses_a_preview_it_cannot_load() {
        // No row identity: the full value can't be fetched, and copying the
        // prefix would silently truncate.
        let nav = previewed_nav(PREVIEW_BYTES as u64 * 4, None);
        assert_eq!(
            plan_copy(&nav, Selection::single((0, 1))),
            Err(CopyRefusal::Unaddressable {
                column: "body".into()
            })
        );
        let message = CopyRefusal::Unaddressable {
            column: "body".into(),
        }
        .message();
        assert!(message.contains("\"body\""), "{message}");
        assert!(message.contains("Export"), "{message}");
    }

    #[test]
    fn copy_summary_names_the_shape_and_format() {
        assert_eq!(
            copy_summary(false, CopyFormat::Csv, 3, 2),
            "Copied 3×2 cells as CSV"
        );
        assert_eq!(
            copy_summary(false, CopyFormat::Tsv { header: true }, 1, 1),
            "Copied 1 cell as TSV with header"
        );
        // The plain shortcut on one cell copies the bare value.
        assert_eq!(
            copy_summary(true, CopyFormat::Tsv { header: false }, 1, 1),
            "Copied the cell value"
        );
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

    // ---- Row detail panel (FRE-109) --------------------------------------

    /// The metadata [`row_detail`] needs beside a [`GridNav`], for a
    /// two-column page (`id` int PK, `title` text) with a foreign key on
    /// `title`.
    struct DetailFixture {
        nav: GridNav,
        meta: TableRenderMeta,
    }

    impl DetailFixture {
        /// [`row_detail`] over this fixture's metadata.
        fn detail(&self, nav: &GridNav, focused: Option<(usize, usize)>) -> Option<RowDetail> {
            row_detail(nav, focused, &self.meta)
        }

        /// Shorthand for the column kinds a [`GridNav`] is built against.
        fn kinds(&self) -> &HashMap<String, (EditorKind, bool)> {
            &self.meta.column_kinds
        }
    }

    /// A two-column table (`id` int PK, `title` text) with a foreign key on
    /// `title`.
    fn detail_table_meta() -> TableMeta {
        let column = |name: &str, type_name: &str, pk: Option<u32>| ColumnMeta {
            name: name.into(),
            type_name: type_name.into(),
            nullable: pk.is_none(),
            primary_key_position: pk,
            default: None,
            generated: Generated::Never,
            type_detail: crate::db::TypeDetail::Plain,
        };
        TableMeta {
            schema: None,
            name: "t".into(),
            kind: crate::db::TableKind::Table,
            columns: vec![
                column("id", "INTEGER", Some(1)),
                column("title", "TEXT", None),
            ],
            indexes: vec![],
            foreign_keys: vec![ForeignKeyMeta {
                columns: vec!["title".into()],
                referenced_schema: None,
                referenced_table: "titles".into(),
                referenced_columns: vec![Some("name".into())],
            }],
            restriction: None,
            internal: None,
            kind_label: None,
        }
    }

    fn detail_fixture() -> DetailFixture {
        let meta = TableRenderMeta::build(Some(&detail_table_meta()), Some(Dialect::Sqlite));
        let result = two_column_result();
        let rows = view_rows(&result, &[], 0, Some(&pk_identity()), None, true);
        let nav = GridNav::build(vec!["id".into(), "title".into()], &rows, &meta.column_kinds);
        DetailFixture { nav, meta }
    }

    #[test]
    fn the_detail_panel_describes_the_focused_row_and_only_that_row() {
        let fixture = detail_fixture();
        // The focused cell's *row* selects the row; its column is irrelevant.
        for focus in [(1, 0), (1, 1)] {
            let detail = fixture.detail(&fixture.nav, Some(focus)).unwrap();
            assert_eq!(
                detail.position,
                DetailPosition {
                    number: 2,
                    total: 2
                }
            );
            assert_eq!(
                detail
                    .fields
                    .iter()
                    .map(|f| f.column.as_str())
                    .collect::<Vec<_>>(),
                ["id", "title"]
            );
            assert_eq!(detail.fields[1].value, Value::Text("two".into()));
            // The whole row travels with it, so an FK jump from any field
            // builds the same filter the grid's ↗ would.
            assert_eq!(detail.row_values["id"], Value::Integer(2));
        }
        // No focus yet (the page just arrived): the first row, not nothing.
        let detail = fixture.detail(&fixture.nav, None).unwrap();
        assert_eq!(detail.position.number, 1);
        assert_eq!(detail.fields[1].value, Value::Text("one".into()));
        // Nothing to describe on an empty page.
        assert!(fixture.detail(&GridNav::default(), Some((0, 0))).is_none());
    }

    #[test]
    fn detail_fields_carry_the_type_the_kind_and_the_grid_s_own_editability() {
        let fixture = detail_fixture();
        let detail = fixture.detail(&fixture.nav, Some((0, 0))).unwrap();
        // Type shown beside the name, via the Schema pane's rendering.
        assert_eq!(detail.fields[0].type_name, "integer");
        assert_eq!(detail.fields[1].type_name, "text");
        // Editor kind and nullability come from the same map the grid uses.
        assert_eq!(detail.fields[1].kind, EditorKind::Text);
        assert!(detail.fields[1].nullable);
        assert!(!detail.fields[0].nullable);
        // A non-NULL FK column offers the jump; a plain column doesn't.
        assert_eq!(
            detail.fields[1]
                .fk
                .as_ref()
                .map(|fk| fk.referenced_table.as_str()),
            Some("titles")
        );
        assert!(detail.fields[0].fk.is_none());
        // Editability is the grid's answer, cell for cell — never re-derived.
        for (field, cell) in detail.fields.iter().zip(&fixture.nav.rows[0].cells) {
            assert_eq!(field.editable, cell.editable, "{}", field.column);
        }
    }

    #[test]
    fn a_read_only_table_offers_no_editors_in_the_panel_either() {
        // `can_mutate = false` is how a read-only marking (FRE-111) reaches
        // the grid; the panel must inherit it rather than resolve its own.
        let fixture = detail_fixture();
        let result = two_column_result();
        let rows = view_rows(&result, &[], 0, Some(&pk_identity()), None, false);
        let nav = GridNav::build(vec!["id".into(), "title".into()], &rows, fixture.kinds());
        let detail = fixture.detail(&nav, Some((0, 0))).unwrap();
        assert!(detail.fields.iter().all(|field| !field.editable));
        // Reading still works: the row stays addressable so previews load.
        assert!(detail.locator.is_some());
    }

    #[test]
    fn a_staged_edit_shows_in_the_panel_as_the_same_change_the_grid_tints() {
        let fixture = detail_fixture();
        let mut stage = TableStage::default();
        stage.set_cell_edit(
            RowLocator {
                identity_values: vec![Value::Integer(1)],
            },
            "title",
            Value::Text("edited".into()),
        );
        let result = two_column_result();
        let rows = view_rows(&result, &[], 0, Some(&pk_identity()), Some(&stage), true);
        let nav = GridNav::build(vec!["id".into(), "title".into()], &rows, fixture.kinds());
        let detail = fixture.detail(&nav, Some((0, 0))).unwrap();
        assert!(detail.fields[1].dirty);
        assert_eq!(detail.fields[1].value, Value::Text("edited".into()));
        assert!(!detail.fields[0].dirty);
        // …and the untouched row is untouched.
        let detail = fixture.detail(&nav, Some((1, 0))).unwrap();
        assert!(detail.fields.iter().all(|field| !field.dirty));
    }

    #[test]
    fn a_previewed_field_is_marked_for_a_full_value_fetch() {
        // The panel must show the whole value, so a truncated cell is flagged
        // for the cell-fetch path rather than rendered from the prefix.
        let fixture = detail_fixture();
        let nav = previewed_nav(PREVIEW_BYTES as u64 * 4, Some(&pk_identity()));
        let detail = fixture.detail(&nav, Some((0, 0))).unwrap();
        let body = detail.fields.iter().find(|f| f.column == "body").unwrap();
        assert!(
            body.preview.is_some(),
            "loaded through load_cell, not shown"
        );
        assert!(detail.locator.is_some(), "…which needs an addressable row");
        // Without an identity the fetch is impossible and the panel says so.
        let nav = previewed_nav(PREVIEW_BYTES as u64 * 4, None);
        let detail = fixture.detail(&nav, Some((0, 0))).unwrap();
        assert!(detail.locator.is_none());
        // A row with no key still needs a stable identity for remounting.
        assert_eq!(detail.row_key, "#0");
    }

    #[test]
    fn prev_and_next_stop_at_the_ends_of_the_page() {
        let ends = DetailPosition {
            number: 1,
            total: 1,
        };
        assert!(!ends.has_prev(), "first row");
        assert!(!ends.has_next(), "…which is also the last");
        let middle = DetailPosition {
            number: 2,
            total: 3,
        };
        assert!(middle.has_prev());
        assert!(middle.has_next());
        assert!(!DetailPosition {
            number: 3,
            total: 3
        }
        .has_next());

        // A step resolves through the grid's own move logic, so it clamps at
        // the page edges exactly as ↑/↓ do rather than wrapping or paging.
        assert_eq!(
            apply_grid_move((0, 1), RowStep::Prev.grid_move(), 3, 2),
            FocusOutcome::Cell((0, 1)),
        );
        assert_eq!(
            apply_grid_move((2, 1), RowStep::Next.grid_move(), 3, 2),
            FocusOutcome::Cell((2, 1)),
        );
        // …and a step in the middle keeps the focused column.
        assert_eq!(
            apply_grid_move((1, 1), RowStep::Next.grid_move(), 3, 2),
            FocusOutcome::Cell((2, 1)),
        );
    }

    #[test]
    fn a_dragged_panel_width_is_clamped_to_something_usable() {
        assert_eq!(clamp_detail_width(400.0), 400.0);
        assert_eq!(clamp_detail_width(10.0), DETAIL_MIN_WIDTH);
        assert_eq!(clamp_detail_width(5_000.0), DETAIL_MAX_WIDTH);
        // A nonsense report from the drag listener falls back to the default
        // rather than writing NaN into the style attribute.
        assert_eq!(clamp_detail_width(f64::NAN), DETAIL_WIDTH);
        assert_eq!(clamp_detail_width(f64::INFINITY), DETAIL_WIDTH);
    }
}
