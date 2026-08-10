//! The paged data grid: the [`DataGrid`] orchestrator and the subsystems it
//! drives, split by concern (FRE-141).
//!
//! - [`rows`] — the per-row/per-cell view model and the components that draw
//!   it;
//! - [`nav`] — keyboard navigation and the windowed row range;
//! - [`copy`] — copying a cell selection to the clipboard;
//! - [`detail`] — the row detail panel;
//! - [`viewer`] — the rich cell viewers (JSON, images, binary, long text).
//!
//! This file keeps the orchestrator itself: the ~20 hooks that hold a table's
//! page, sort, filter, selection and stage, the effects that keep them
//! consistent, and the toolbar/save-bar/footer chrome around the body. The
//! five submodules reference each other only through small shared types
//! ([`GridNav`], [`Selection`], [`ActiveEdit`], [`RowDetail`]), which live
//! here.
//!
//! The submodules open with `use super::*`: they are one component tree split
//! across files rather than modules with a surface of their own, so they
//! inherit this file's imports instead of restating them four times.

mod copy;
mod detail;
mod nav;
mod rows;
mod viewer;

use copy::*;
use detail::*;
use nav::*;
use rows::*;
use viewer::*;

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
use super::js::{focus_by_id_next_frame, js_string};
use super::notice::{Banner, BannerKind, DelayedLoading, EmptyState};
use super::schema::display_type;
use super::selection::Selection;
use super::stage::{required_insert_columns, PendingInsert, TableStage};
use super::state::{find_table_meta, AppState, ExportPane, ExportStatus, TableRef};

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
    /// Carries the [`Value`], not its display string: the popup renders it
    /// through [`CellViewer`] (FRE-115), and "Copy raw" has to put the same
    /// text on the clipboard as a Ctrl+C over that cell —
    /// [`raw_cell_text`], i.e. the blob's hex and nothing at all for NULL.
    /// Holding only the display string copied the placeholder.
    ///
    /// The column travels with it so the popup can look up the declared type
    /// the viewer classifies on, exactly as the detail panel does.
    Text { value: Value, column: String },
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
        (false, _) => ExpandView::Text {
            value: cell.value.clone(),
            column: cell.column.clone(),
        },
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
    let filter_column = use_signal(String::new);
    let filter_op = use_signal(|| FilterOp::Contains);
    let filter_text = use_signal(String::new);
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
        let meta = find_table_meta(schemas.get(&id), &meta_table);
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

    // Whether this object has any rows to list at all (FRE-148), so that a
    // declared-unbrowsable object never reaches the server: the query it would
    // otherwise run is one the engine answers with an error about an object
    // that stores nothing. A RisingWave sink is the case — it writes outward
    // to Kafka or another database.
    //
    // Resolved through a memo for the reason `fetch_meta` above is: `registry`
    // and `schemas` are whole-map signals, so a raw read would make every
    // other connection's open, close or protection change re-run this grid's
    // page fetch and its `COUNT(*)` (FRE-129). `Option<Option<_>>` because
    // "unknown" and "browsable" are different answers — a reload sets
    // `SchemaLoad::Loading`, which empties the table list, so the outer `None`
    // is the ordinary state during a sidebar Refresh rather than an edge case.
    let gate_table = table.clone();
    let resolved_gate = use_memo(move || {
        let registry = state.registry.read();
        let schemas = state.schemas.read();
        match (
            find_table_meta(schemas.get(&id), &gate_table),
            registry.get(id),
        ) {
            (Some(meta), Some(connection)) => Some(connection.access(meta).unreadable),
            _ => None,
        }
    });
    let mut unbrowsable = use_signal(|| resolved_gate().flatten());
    use_effect(move || {
        // Keeps the last known answer while the schema reloads, instead of
        // falling open and sending the very query the gate exists to stop —
        // the same treatment the SQL editor gives its completions. A table
        // does not stop being a sink because its schema is being re-read.
        //
        // Covers reloads, not the first load: a grid mounted by session
        // restore before its schema has ever arrived has no answer to keep, so
        // one refused query can still go out and be corrected when the schema
        // lands. Self-correcting, because the resources read this signal.
        //
        // Peeked before writing: `Signal::set` is unconditional, and both
        // resources subscribe to this, so setting an unchanged value would
        // re-fetch the page and re-count the rows for nothing — undoing the
        // memo's gate one line above.
        if let Some(reason) = resolved_gate() {
            if *unbrowsable.peek() != reason {
                unbrowsable.set(reason);
            }
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
        let refused = unbrowsable();
        // Peeked, not read: subscribing to `registry` here would re-fetch on
        // any connection open/close. The pool for a ConnectionId never
        // changes while this grid is mounted — the registry's only writes
        // are `insert` (mints a fresh id), `set_protection` (pool untouched)
        // and `remove` (which unmounts this tab) — so there is no pool
        // change to react to; a reconnect is a new id and a new grid.
        let pool = state.registry.peek().get(id).map(|c| c.pool.clone());
        async move {
            if let Some(reason) = refused {
                return Err(crate::db::DbError::Unsupported(reason.to_string()));
            }
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
        let refused = unbrowsable();
        // Peeked for the same reason as in `rows_resource`: the pool for a
        // ConnectionId is fixed for the connection's lifetime, and reading
        // `registry` would re-count on unrelated connection opens/closes.
        let pool = state.registry.peek().get(id).map(|c| c.pool.clone());
        async move {
            // Gated as well as the page: counting rows an object does not have
            // is the same query failing for the same reason.
            if let Some(reason) = refused {
                return Err(crate::db::DbError::Unsupported(reason.to_string()));
            }
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
            find_table_meta(schemas.get(&id), &render_table),
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
    // a memo none of those reach it: a focus move now costs the two `GridRow`
    // diffs it should.
    //
    // It is not fully insulated, and doesn't need to be. `schemas`, `registry`
    // and `stages` are whole-map signals, so this still re-runs on an
    // unrelated connection's schema load or save — as the render body always
    // did. What changed is that the PartialEq gate below stops such a rebuild
    // from reaching the rows, so the cost is one derivation rather than a
    // re-render of the grid.
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
        let access = find_table_meta(state.schemas.read().get(&id), &page_table)
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
            focus_by_id_next_frame("dv-grid");
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
    let table_meta: Option<TableMeta> = state.table_meta(id, &table);

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
    // Whether this object has rows to list at all (FRE-148). Absent access
    // means the schema has not loaded yet, which is not a refusal.
    let can_read = access.as_ref().is_none_or(TableAccess::can_read);

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

    let current = rows_resource.read();
    let sort_value = sort();
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
            // Filter bar (FRE-141: its own component so a copy or export
            // finishing repaints the bar, not the grid body).
            GridToolbar {
                id,
                table: table.clone(),
                dialect,
                can_read,
                detail_open,
                meta: meta.clone(),
                filter_column,
                filter_op,
                filter_text,
                applied_filter,
                page,
                sort,
                selection,
                grid_nav,
                copy_status,
                copy_menu,
            }
            GridBars {
                id,
                table: table.clone(),
                pending_count,
                delete_count,
                armed,
                saving,
                save_error,
                missing_required,
                can_mutate,
                selected,
                confirm,
                read_only_notice,
            }
            // Read-only notice (views / no usable row key). Suppressed for an
            // object with no rows at all: its restriction explains both why it
            // cannot be edited and why it cannot be opened, and the grid below
            // is already showing that same sentence — twice on one screen
            // reads as two problems (FRE-148).
            if let Some(notice) = read_only_notice.filter(|_| can_read) {
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
                        // An object the backend declares has no rows (FRE-148)
                        // is not a failure to report in red: nothing went
                        // wrong, and there is simply nothing to list. A
                        // RisingWave sink is the case — it writes outward to
                        // Kafka or another database and stores nothing itself.
                        // The sentence is the backend's own, so it explains the
                        // object rather than naming the engine.
                        //
                        // Guarded on the error alone would be wrong:
                        // `Unsupported` is also how `refuse_paged_read` reports
                        // a connection that cannot query or cannot page by
                        // offset, and rendering *that* as "nothing to browse
                        // here" would be a false claim about the object. No
                        // backend declares either today, which is exactly why
                        // the arm has to say which case it means.
                        //
                        // The guard reads the *same* signal the resources
                        // refused on, not the resolved `can_read`: that one
                        // comes back through `find_table_meta`, which reports
                        // nothing while the schema reloads — so during a
                        // Refresh it would say "readable" exactly when the
                        // resource is returning this error, and the empty state
                        // would flip to a red banner.
                        Some(Err(crate::db::DbError::Unsupported(reason)))
                            if unbrowsable().is_some() =>
                        rsx! {
                            EmptyState {
                                icon: rsx! { File { size: 40 } },
                                title: "Nothing to browse here",
                                hint: "{reason}",
                            }
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
                                                onclick: move |_| clear_filter(
                                                    filter_text,
                                                    applied_filter,
                                                    page,
                                                ),
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
            GridFooter { rows_resource, count_resource, page }
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
                            // value, so the rich viewers may decode it: JSON
                            // as a tree, an image as a picture (FRE-77/FRE-115).
                            ExpandView::Text { value, column } => {
                                let type_name = meta.type_of(&column);
                                rsx! {
                                    CopyRawButton { value: value.clone() }
                                    CellViewer { value, type_name, truncated: None }
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
                            ExpandView::Fetch { locator, column } => {
                                let type_name = meta.type_of(&column);
                                rsx! {
                                    ExpandedValue { id, table: table.clone(), locator, column, type_name }
                                }
                            },
                        }
                    }
                }
            }
        }
    }
}

/// The grid's toolbar: FK Back, the column/op/value filter, the copy-as menu,
/// the export buttons, the row-detail toggle and Refresh.
///
/// Its own component so a copy or export finishing repaints this bar alone
/// (FRE-141). `copy_status`, the export status and the selection's shape are
/// read *here* rather than in [`DataGrid`] — read up there, every copy would
/// re-render the whole grid body.
#[component]
fn GridToolbar(
    id: ConnectionId,
    table: TableRef,
    /// `None` until the connection reports one; gates the INSERT copy format
    /// and the export buttons, which both need to know the dialect.
    dialect: Option<Dialect>,
    /// Whether this object has rows to read (FRE-148). False hides the export
    /// affordances: an export is the same query the grid is refusing, so
    /// offering it would hand back the failure the gate just prevented.
    can_read: bool,
    /// Whether the row detail panel is docked — the toggle's pressed state.
    /// A prop rather than a read, because [`DataGrid`] needs it for the body
    /// layout anyway.
    detail_open: bool,
    meta: Shared<TableRenderMeta>,
    filter_column: Signal<String>,
    filter_op: Signal<FilterOp>,
    filter_text: Signal<String>,
    applied_filter: Signal<Option<Filter>>,
    page: Signal<u64>,
    sort: Signal<Option<(String, SortDir)>>,
    selection: ReadSignal<Option<Selection>>,
    grid_nav: Memo<GridNav>,
    copy_status: Signal<Option<CopyStatus>>,
    copy_menu: Signal<bool>,
) -> Element {
    let state = use_context::<AppState>();
    // Whether the FK Back stack has anywhere to return to (reactive).
    let can_back = state.can_go_back(id);
    let export_status: Option<ExportStatus> = state
        .export_status
        .read()
        .get(&(id, ExportPane::Grid))
        .cloned();
    let export_table = table.clone();
    let refresh_table = table.clone();
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
    rsx! {
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
                    onclick: move |_| clear_filter(filter_text, applied_filter, page),
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
            if let Some(export_dialect) = dialect.filter(|_| can_read) {
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
    }
}

/// The grid's footer: which rows this page covers, of how many, and the page
/// stepper.
///
/// Reads the two resources itself rather than taking their values: the row
/// range and the Next button's bound are derived from both at once, and the
/// derivation is the only thing here worth naming.
#[component]
fn GridFooter(
    rows_resource: Resource<Result<(Page, Option<String>), crate::db::DbError>>,
    count_resource: Resource<Result<u64, crate::db::DbError>>,
    page: Signal<u64>,
) -> Element {
    let current = rows_resource.read();
    rsx! {
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
    }
}

/// The bars between the toolbar and the rows: pending changes with
/// Save/Discard, the write-protection confirmation (FRE-111), and the
/// selection's Delete bar.
///
/// Its own component so the three signals only this chrome reads — the
/// navigation guard, the parked-save confirmation and the connection's name —
/// stop re-rendering the grid body when they change. The stage-derived counts
/// stay props: [`DataGrid`] reads the stage anyway to tint the rows.
#[component]
fn GridBars(
    id: ConnectionId,
    table: TableRef,
    /// Staged changes on this table, and how many of them are deletes.
    pending_count: usize,
    delete_count: usize,
    /// Whether the exact-count delete confirmation is armed.
    armed: bool,
    saving: bool,
    save_error: Option<String>,
    missing_required: usize,
    can_mutate: bool,
    /// Rows ticked for deletion but not yet staged.
    selected: Signal<HashMap<String, RowLocator>>,
    confirm: Signal<Option<Vec<StagedChange>>>,
    /// Why this table refuses writes, if it does — named in the Save bar so a
    /// disabled Save button is never unexplained.
    read_only_notice: Option<&'static str>,
) -> Element {
    let state = use_context::<AppState>();
    // Read here, not in `DataGrid`: these three are what this chrome is for.
    //
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
    // The two-step navigation guard parks blocked navigations here; the Save
    // bar explains how to proceed (see AppState::nav_guard for the UX).
    let nav_blocked = state
        .nav_guard
        .read()
        .as_ref()
        .is_some_and(|nav| nav.id == id);
    let save_table = table.clone();
    let discard_table = table.clone();
    let delete_table = table.clone();
    let confirm_save_table = table.clone();
    let dismiss_save_table = table.clone();
    rsx! {
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

/// Clears the filter and returns to the first page — what both "Clear"
/// affordances do (the toolbar's button and the no-rows-match empty state).
///
/// Both the applied filter and the input text go: leaving the text behind
/// would show a filter that isn't in effect.
fn clear_filter(
    mut filter_text: Signal<String>,
    mut applied_filter: Signal<Option<Filter>>,
    mut page: Signal<u64>,
) {
    applied_filter.set(None);
    filter_text.set(String::new());
    page.set(0);
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

/// Fixtures shared by the grid submodules' tests. Lives here because more
/// than one of them builds a page against the same two-column table.
#[cfg(test)]
mod fixtures {
    use super::*;

    /// A two-column table (`id` int PK, `title` text) with a foreign key on
    /// `title`.
    pub(super) fn detail_table_meta() -> TableMeta {
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

    pub(super) fn two_column_result() -> QueryResult {
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

    pub(super) fn pk_identity() -> RowIdentity {
        RowIdentity::PrimaryKey {
            columns: vec!["id".into()],
        }
    }

    /// A one-row page whose `body` cell is a truncated preview of `full_len`
    /// (FRE-110 copy planning); `identity` decides whether its row can be
    /// addressed to load the full value.
    pub(super) fn previewed_nav(full_len: u64, identity: Option<&RowIdentity>) -> GridNav {
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
}

#[cfg(test)]
mod tests {
    use super::fixtures::*;
    use super::*;
    use crate::db::PREVIEW_BYTES;

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
        let ExpandView::Text { value, column } = expand_view(blob, nav.rows[0].locator.clone())
        else {
            panic!("a complete value expands in hand");
        };
        assert_eq!(raw_cell_text(&value), "\\xdead");
        // The column travels with the value so the popup can classify it on
        // the declared type, like the detail panel (FRE-115).
        assert_eq!(column, "cover");
        // Same for NULL: the popup reads "NULL", a copy yields nothing —
        // exactly as `plan_copy` + `raw_cell_text` do for the same cell.
        let ExpandView::Text { value, .. } = expand_view(&nav.rows[0].cells[0], None) else {
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
