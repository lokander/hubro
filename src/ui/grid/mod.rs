//! The paged data grid: the [`DataGrid`] orchestrator and the subsystems it
//! drives, split by concern.
//!
//! - [`page`] — the page, sort and filter asked for, the page and count
//!   fetched for them, and the paging rules;
//! - [`focus`] — the focus ring, the cell selection, the two editors that can
//!   take the keyboard, and the key handler;
//! - [`body`] — the windowed rows and the states shown in their place;
//! - [`expand`] — the value-expand popup;
//! - [`rows`] — the per-row/per-cell view model and the components that draw
//!   it;
//! - [`nav`] — keyboard navigation and the windowed row range, as rules;
//! - [`copy`] — copying a cell selection to the clipboard;
//! - [`stats`] — the footer's aggregate readout for a cell selection;
//! - [`detail`] — the row detail panel;
//! - [`viewer`] — the rich cell viewers (JSON, images, binary, long text).
//!
//! This file keeps the orchestrator itself — the hooks called in order, the
//! table's access and stage read once, and the layout — and the chrome
//! around the body: the toolbar, the save bars and the footer. The
//! submodules reference each other only through small shared types
//! ([`GridNav`], [`Selection`], [`ActiveEdit`], [`RowDetail`], and the
//! handle bundles [`GridQuery`], [`GridPage`], [`GridFocus`]), which live
//! beside the code that builds them.
//!
//! The submodules open with `use super::*`: they are one component tree split
//! across files rather than modules with a surface of their own, so they
//! inherit this file's imports instead of restating them every time.

mod body;
mod copy;
mod detail;
mod expand;
mod focus;
mod nav;
mod page;
mod rows;
mod stats;
mod viewer;

use body::*;
use copy::*;
use detail::*;
use expand::*;
use focus::*;
use nav::*;
use page::*;
use rows::*;
use stats::*;
use viewer::*;

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use dioxus::prelude::*;
use dioxus_icons::lucide::{
    ChevronDown, ChevronUp, File, PanelRight, Pencil, RefreshCw, SearchX, ShieldAlert, X,
};

use crate::db::{
    import_refusal, raw_cell_text, render_copy, CellFetch, ColumnMeta, ConnectionId, CopyBlock,
    CopyFormat, Dialect, ExportFormat, Filter, FilterOp, ForeignKeyMeta, Generated, Page,
    PageRequest, PreviewInfo, QueryResult, RowIdentity, RowLocator, SortDir, StagedChange,
    TableAccess, TableMeta, Value, FETCH_CELL_MAX_BYTES,
};
use crate::ui::classes::{BTN_GHOST, INPUT, SELECT};
use crate::util::human_bytes;

use super::editing::{editor_kind, CellEditor, EditNav, EditorKind};
use super::import::ImportDialog;
use super::js::{
    focus_by_id_next_frame, focus_on_mount, reclaim_focus_by_id_next_frame, write_clipboard,
};
use super::notice::{
    a_held_key_is_not_a_press, Armed, Banner, BannerKind, DelayedLoading, EmptyState, Gesture,
    Overlay, OverlayAlign,
};
use super::pick_export_path;
use super::schema::display_type;
use super::selection::Selection;
use super::shell::accel_held;
use super::stage::{required_insert_columns, PendingInsert, TableStage};
use super::state::{
    find_table_meta, fk_jump_target, grid_export_line, AppState, ImportStatus, TableRef,
};
use super::text_field::TextInput;

/// Render data shared by many components, cloned by refcount rather than
/// deep-copied into every one of them.
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
    /// scrolled out of the virtual window while its input didn't parse;
    /// the remounting editor seeds from it instead of the cell
    /// value.
    draft: Option<String>,
}

impl ActiveEdit {
    /// Whether this open editor is the one for `column` of `row_key`.
    ///
    /// The row half matters for the detail panel: its editor is
    /// owned above the row-keyed field list so a draft survives a row move,
    /// which means an editor left open on one row would otherwise reappear on
    /// a same-named field of the next one, seeded with the wrong row's text.
    fn is_on(&self, row_key: &str, column: &str) -> bool {
        self.row_key == row_key && self.column == column
    }
}

/// Paged grid for one table: sortable headers, per-column contains/equals
/// filter, page navigation, row-count indicator, refresh — plus staged-edit
/// rendering: dirty cells and deletes are tinted, pending inserts
/// show as phantom rows, and a Save/Discard bar appears while the table's
/// stage is non-empty. Cells of editable rows open a type-aware in-place
/// editor (see [`CellEditor`]) on double-click or Enter; commits
/// only ever stage (never write through).
///
/// Row-level staging, all gated on the table being editable (a
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
///
/// The state lives in hooks named for what they hold — [`use_grid_query`]
/// and [`use_grid_page`] (the page), [`use_grid_editors`], [`use_grid_focus`]
/// and [`use_selection`] (the keyboard), [`use_row_window`] and
/// [`use_visible_rows`] (the windowed rows) — called in the order their
/// effects have to be installed. What is left here is reading the table's
/// access and stage, and laying out the chrome around [`GridBody`].
#[component]
pub fn DataGrid(id: ConnectionId, table: TableRef) -> Element {
    let state = use_context::<AppState>();
    let editors = use_grid_editors();
    let editing = editors.editing;
    let focus = use_grid_focus();
    let GridFocus {
        focused_cell,
        copy_status,
        ..
    } = focus;
    // Whether the copy-as menu is open.
    let copy_menu = use_signal(|| false);
    // The value-expand popup: either an already-known short value
    // rendered inline, or a truncated cell whose full value is fetched on
    // demand.
    let expanded = use_signal(|| Option::<ExpandView>::None);
    let window = use_row_window();
    // Rows ticked for deletion (row key → locator). UI-only state: nothing
    // is staged until "Delete N selected".
    let selected = use_signal(HashMap::<String, RowLocator>::new);
    let confirm = use_save_confirm(id, &table);
    let query = use_grid_query(id, &table);
    use_grid_reset(
        query,
        editing,
        focus,
        copy_menu,
        expanded,
        selected,
        window.scroll_top,
    );
    let grid_page = use_grid_page(id, &table, query);
    let grid_nav = grid_page.nav;
    let render_meta = grid_page.render_meta;
    let selection = use_selection(focus, grid_nav);
    let visible_range = use_visible_rows(window, grid_nav);
    use_editor_refocus(editors);

    // The row detail panel's model. The panel's row is derived from
    // the selection's focus, never stored: it cannot drift from the cell the
    // grid has focused.
    //
    // Memoized: it clones the focused row's every value, and the
    // grid re-renders on every stage change and every panel toggle.
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
    // even for zero-row results), foreign keys drive the clickable FK cells,
    // and the editor kinds decide what each cell may open.
    let meta = render_meta();
    // Row-identity detection decides the read-only notice and how staged rows
    // are addressed.
    let table_meta: Option<TableMeta> = state.table_meta(id, &table);

    let dialect: Option<Dialect> = state.registry.read().get(id).map(|c| c.pool.dialect());
    // The connection's capabilities resolved for this table, narrowed by the
    // connection's write protection: one answer for whether editing is
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
    // Whether this object has rows to list at all. Absent access
    // means the schema has not loaded yet, which is not a refusal.
    let can_read = access.as_ref().is_none_or(TableAccess::can_read);
    // Why importing a file into this object is refused, when it is.
    // Resolved from the same `access` as every other write affordance, so an
    // import is offered exactly where editing is.
    let import_refused: Option<String> =
        access.as_ref().and_then(import_refusal).map(str::to_string);
    // The file the user picked, if the import dialog is open over this grid.
    let mut import_file = use_signal(|| Option::<std::path::PathBuf>::None);
    // The connection's name when it is marked confirm-writes, so
    // the dialog can name it in the confirmation an import has to pass.
    let confirm_connection: Option<String> = state
        .registry
        .read()
        .get(id)
        .filter(|c| c.confirms_writes())
        .map(|c| c.name.clone());

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
    // the stage exactly (see `use_save_confirm`).
    let armed = delete_count > 0
        && stage.as_ref().is_some_and(|s| {
            confirm
                .read()
                .as_ref()
                .map(|armed| armed.subject().as_slice())
                == Some(s.changes().as_slice())
        });

    let row_table = table.clone();
    let detail_table = table.clone();
    // The row detail panel. Open/closed and width live on the tab,
    // not here, so they survive a table switch (which remounts this grid) —
    // and the open flag rides the persisted session.
    let detail_open = state.row_detail_open(id);
    let detail_width = clamp_detail_width(state.row_detail_width(id).unwrap_or(DETAIL_WIDTH));
    // Everything a clipboard copy needs besides the selection itself.
    let copy_ctx = CopyContext {
        state,
        id,
        table: table.clone(),
        dialect,
        status: copy_status,
    };
    let on_grid_key = grid_key_handler(
        editing, expanded, focus, copy_menu, copy_ctx, grid_page, query.page,
    );
    // What every row renders against, handed to each as one prop.
    let row_ctx = RowContext {
        id,
        table: table.clone(),
        meta: meta.clone(),
        dialect: dialect.unwrap_or(Dialect::Sqlite),
        editing,
    };
    let pending_inserts: Vec<PendingInsert> = stage
        .as_ref()
        .map(|s| s.inserts().to_vec())
        .unwrap_or_default();
    let grid_selection = GridSelection {
        selection,
        grid_nav,
        copy_status,
        copy_menu,
    };
    let applied_filter = query.filter.applied;

    rsx! {
        div { class: "flex h-full min-h-0 flex-col",
            // Filter bar (its own component so a copy or export
            // finishing repaints the bar, not the grid body).
            GridToolbar {
                id,
                table: table.clone(),
                dialect,
                can_read,
                can_import: table_meta.is_some(),
                import_refused: import_refused.clone(),
                import_file,
                detail_open,
                meta: meta.clone(),
                query,
                selection: grid_selection,
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
            // object with no rows to show: its restriction explains both why it
            // cannot be edited and why it cannot be opened, and the grid below
            // is already showing that same sentence — twice on one screen
            // reads as two problems.
            if let Some(notice) = read_only_notice.filter(|_| can_read) {
                div { class: "px-3 pt-2",
                    Banner { kind: BannerKind::Info, message: notice.to_string() }
                }
            }
            // The rows and, docked to their right, the row detail panel.
            // The toolbar and footer above and below span both:
            // they describe the table, while the panel describes one row.
            div { class: "flex min-h-0 flex-1",
                // Grid — a single focusable region (tabindex 0) so arrow-key cell
                // navigation works without per-cell tab stops. Focused on
                // mount so the ring responds immediately; `outline-none` since the
                // ring itself signals focus.
                div {
                    id: "dv-grid",
                    // `select-none`: shift-click extends the *cell*
                    // selection, and without this the webview drags its own text
                    // highlight across the rows at the same time, striping the
                    // grid. The expand popup renders outside this container, so
                    // its text stays selectable.
                    class: "min-h-0 flex-1 select-none overflow-auto outline-none",
                    tabindex: "0",
                    onkeydown: on_grid_key,
                    GridBody {
                        ctx: row_ctx,
                        grid: grid_page,
                        query,
                        focus,
                        selection,
                        visible_range,
                        select_enabled,
                        selected,
                        inserts: pending_inserts,
                    }
                }
                if detail_open {
                    RowDetailPanel {
                        id,
                        table: detail_table,
                        detail: detail(),
                        on_pending_insert: focused_cell().is_some_and(|(row, _)| row >= grid_nav.read().rows.len()),
                        width: detail_width,
                        dialect: dialect.unwrap_or(Dialect::Sqlite),
                        read_only_notice: read_only_notice.map(str::to_string),
                        editors,
                        on_step: move |step: RowStep| focus.step_row(grid_nav, step),
                        on_close: move |_| state.set_row_detail(id, false),
                        on_width: move |width: f64| state.set_row_detail_width(id, width),
                        on_fk_jump: {
                            let origin = row_table.clone();
                            move |(fk, row_values, gesture): (ForeignKeyMeta, HashMap<String, Value>, Gesture)| {
                                state.navigate_fk(id, &fk, &row_values, &origin, applied_filter.peek().clone(), gesture);
                            }
                        },
                    }
                }
            }
            // Footer: paging + counts
            GridFooter { grid: grid_page, page: query.page, selection: grid_selection }
            // Value-expand popup: Enter on a read-only / non-editable
            // focused cell shows its full value — fetched on demand for a
            // truncated cell.
            ExpandPopup { id, table: table.clone(), meta: meta.clone(), expanded }
            // The import dialog, open once a file has been picked.
            // Mounted here rather than in the toolbar because it needs the
            // table's columns, which this scope already has.
            if let (Some(path), Some(meta)) = (import_file(), table_meta.clone()) {
                ImportDialog {
                    id,
                    table: meta,
                    path,
                    refusal: import_refused.clone(),
                    confirm_connection: confirm_connection.clone(),
                    on_close: move |_| import_file.set(None),
                }
            }
        }
    }
}

/// Closes any open editor and drops the row selection when the rows change
/// under them: a page flip, sort/filter change, or refetch replaces the
/// grid's contents — a stale ActiveEdit would spontaneously re-open the
/// editor if its row key scrolled back into view, and a stale selection
/// could stage deletes for rows the user no longer sees.
fn use_grid_reset(
    query: GridQuery,
    mut editing: Signal<Option<ActiveEdit>>,
    focus: GridFocus,
    mut copy_menu: Signal<bool>,
    mut expanded: Signal<Option<ExpandView>>,
    mut selected: Signal<HashMap<String, RowLocator>>,
    mut scroll_top: Signal<f64>,
) {
    let GridQuery {
        page,
        sort,
        filter,
        refresh: refresh_nonce,
    } = query;
    let applied_filter = filter.applied;
    let GridFocus {
        mut focused_cell,
        mut selection_anchor,
        mut copy_status,
    } = focus;
    use_effect(move || {
        let _ = page();
        let _ = sort.read();
        let _ = applied_filter.read();
        let _ = refresh_nonce();
        // Peeked first: a set is unconditional, and every write wakes the
        // focus effect in `use_editor_refocus` — which is how applying a
        // filter used to pull focus out of the filter box.
        if editing.peek().is_some() {
            editing.set(None);
        }
        selected.set(HashMap::new());
        // Re-seed the focus ring at the first cell and drop any expand popup;
        // the clamp effect in `use_selection` trims it to None for an empty
        // page.
        focused_cell.set(Some((0, 0)));
        // …and collapse the cell selection to it: the rows under a
        // rectangle spanning the old page are not the rows the user picked.
        selection_anchor.set(None);
        copy_status.set(None);
        copy_menu.set(false);
        expanded.set(None);
        // Reset the windowed scroll position too so a new page/sort/
        // filter starts at the top, both the tracked offset and the container.
        scroll_top.set(0.0);
        document::eval(
            "requestAnimationFrame(() => { \
                const el = document.getElementById('dv-grid'); \
                if (el) el.scrollTop = 0; \
            });",
        );
        // A reset can take away whatever held focus: Save's button goes with
        // the pending-changes bar once the stage empties, and a refresh
        // closes an open value popup. Either leaves focus on the body, so
        // hand it back — but only from there, never from the filter box a
        // filter was just applied from, nor from the Refresh button.
        reclaim_focus_by_id_next_frame("dv-grid");
    });
}

/// The armed save confirmation: the exact change list shown to the user
/// when the Save button turned into "Confirm: delete N…". The second
/// click only proceeds while the stage still generates this exact list.
fn use_save_confirm(
    id: ConnectionId,
    table: &TableRef,
) -> Signal<Option<Armed<Vec<StagedChange>>>> {
    let state = use_context::<AppState>();
    let mut confirm = use_signal(|| Option::<Armed<Vec<StagedChange>>>::None);
    // Once armed, the FIRST divergence of the stage from the snapshot
    // permanently disarms — clearing `confirm`, not just hiding it. Without
    // this, staging then un-staging back to an identical change list (e.g.
    // add a phantom insert, then remove it) would silently re-arm the
    // confirmation, letting the next single click save without a fresh
    // confirm step. Requiring a new first click after any change keeps the
    // "two clicks per confirmation instance" guarantee.
    let confirm_reset_table = table.clone();
    use_effect(move || {
        let Some(snapshot) = confirm.read().as_ref().map(|armed| armed.subject().clone()) else {
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
    confirm
}

/// Opens a file picker and, on a chosen file, opens the import dialog over
/// the grid.
///
/// The picker does not block the window, so the grid can unmount while it is
/// open (a table switch). A component-scoped `spawn` was cancelled with it:
/// the picker stayed open, accepted a file, and nothing happened, anywhere —
/// measured. So the task outlives the grid, and a file picked for a grid that
/// is gone (its `target` signal dropped with it) is reported as a cancelled
/// import instead ([`AppState::report_import_pick_dropped`], which says why
/// it is not carried over).
fn pick_import_file(
    state: AppState,
    id: ConnectionId,
    table: TableRef,
    target: Signal<Option<std::path::PathBuf>>,
) {
    dioxus::core::spawn_forever(async move {
        let picked = rfd::AsyncFileDialog::new()
            .set_title("Import into this table")
            .add_filter(
                "CSV and JSON",
                &["csv", "tsv", "txt", "json", "jsonl", "ndjson"],
            )
            .add_filter("All files", &["*"])
            .pick_file()
            .await;
        if let Some(file) = picked {
            deliver_import_pick(state, id, &table, target, file.path().to_path_buf());
        }
    });
}

/// Hands a picked import file to the grid that asked for it — or, when that
/// grid has unmounted, reports the pick as a cancelled import. Its `target`
/// signal went with it and refuses the write, which is how a gone grid is
/// told from a live one.
///
/// Split out of [`pick_import_file`] so it can be tested: dropping the
/// report here is exactly the silent loss the task's `spawn_forever` exists
/// to end, and the dialog in front of it cannot run in a test.
fn deliver_import_pick(
    state: AppState,
    id: ConnectionId,
    table: &TableRef,
    mut target: Signal<Option<std::path::PathBuf>>,
    path: std::path::PathBuf,
) {
    match target.try_write() {
        Ok(mut slot) => *slot = Some(path),
        Err(_) => state.report_import_pick_dropped(id, table.label()),
    }
}

/// The skipped lines a finished import reported, as one line — `None` when
/// it skipped nothing. The status text says *how many*; this says *which*,
/// because a count alone leaves the user with no way to find the rows that
/// did not arrive.
fn import_skipped(status: &ImportStatus) -> Option<String> {
    let report = status.report()?;
    if report.skipped.is_empty() {
        return None;
    }
    let mut lines: Vec<String> = report
        .skipped
        .iter()
        .map(|skipped| format!("line {}: {}", skipped.line, skipped.reason))
        .collect();
    if report.skips_truncated() {
        lines.push(format!(
            "and {} more",
            report.skipped_rows - report.skipped.len() as u64
        ));
    }
    Some(lines.join(" · "))
}

/// The grid's toolbar: FK Back, the column/op/value filter, the copy-as menu,
/// the export buttons, the row-detail toggle and Refresh.
///
/// Its own component so a copy or export finishing repaints this bar alone.
/// `copy_status`, the export status and the selection's shape are
/// read *here* rather than in [`DataGrid`] — read up there, every copy would
/// re-render the whole grid body.
#[component]
fn GridToolbar(
    id: ConnectionId,
    table: TableRef,
    /// `None` until the connection reports one; gates the INSERT copy format
    /// and the export buttons, which both need to know the dialect.
    dialect: Option<Dialect>,
    /// Whether this object has rows to read. False hides the export
    /// affordances: an export is the same query the grid is refusing, so
    /// offering it would hand back the failure the gate just prevented.
    can_read: bool,
    /// Whether an import can be offered at all: false until the schema has
    /// loaded, since the dialog maps fields onto the table's columns and has
    /// none to show without it.
    can_import: bool,
    /// Why an import is refused, when it is — the same sentence the
    /// disabled Save button shows. `Some` leaves the button visible but
    /// disabled and explained, rather than silently absent.
    import_refused: Option<String>,
    /// The file picked for import, if any: set by this toolbar's picker and
    /// consumed by [`DataGrid`], which owns the dialog.
    import_file: Signal<Option<std::path::PathBuf>>,
    /// Whether the row detail panel is docked — the toggle's pressed state.
    /// A prop rather than a read, because [`DataGrid`] needs it for the body
    /// layout anyway.
    detail_open: bool,
    meta: Shared<TableRenderMeta>,
    /// The filter inputs this bar edits, and the sort and filter an export
    /// writes the view under.
    query: GridQuery,
    /// The cell selection the copy-as menu copies.
    selection: GridSelection,
) -> Element {
    let state = use_context::<AppState>();
    let sort = query.sort;
    let GridFilter {
        column: mut filter_column,
        op: mut filter_op,
        text: mut filter_text,
        applied: applied_filter,
    } = query.filter;
    let GridSelection {
        selection,
        grid_nav,
        copy_status,
        mut copy_menu,
    } = selection;
    // Whether the FK Back stack has anywhere to return to (reactive).
    let can_back = state.can_go_back(id);
    let export_line = grid_export_line(
        &state.export_status.read(),
        &state.grid_export_tables.read(),
        id,
        &table,
    );
    let import_status: Option<ImportStatus> = state.import_status.read().get(&id).cloned();
    let export_table = table.clone();
    let refresh_table = table.clone();
    // Everything a clipboard copy needs besides the selection itself.
    // Cloned per event handler; all of it is cheap.
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
        div { class: "border-b border-slate-200 dark:border-slate-800",
        // An import reports on a line of its own rather than in the row
        // below. That row is routinely over its width — the export buttons
        // already wrap — so a `truncate`d status there collapses to nothing
        // (it is the only shrinkable item), and an import's message is a
        // sentence naming a line and a column, not a word.
        if let Some(status) = import_status.as_ref() {
            {
                let (text, tone) = status.line();
                let skipped = import_skipped(status);
                rsx! {
                    div { class: "flex items-start gap-2 border-b border-slate-200 dark:border-slate-800 px-3 py-1.5 text-xs",
                        // Wraps rather than clips: a failure or a cancelled
                        // pick is a sentence, wider than the row at a normal
                        // window width.
                        span { class: "min-w-0 break-words font-medium {tone.class()}", "{text}" }
                        if let Some(skipped) = skipped {
                            span { class: "min-w-0 flex-1 truncate text-slate-500 dark:text-slate-400",
                                title: "{skipped}",
                                "{skipped}"
                            }
                        }
                        div { class: "flex-1" }
                        button {
                            class: "shrink-0 rounded px-1.5 py-0.5 text-slate-500 dark:text-slate-400 hover:bg-slate-200 dark:hover:bg-slate-800",
                            title: "Dismiss",
                            onclick: move |_| state.clear_import_status(id),
                            "Dismiss"
                        }
                    }
                }
            }
        }
        // The export reports on a line of its own too, for the same reason:
        // in the toolbar row it was the shrinkable item, and at a normal
        // window width "Exported 500000 rows" truncated to nothing — so an
        // export finished with no visible outcome at all.
        if let Some((text, tone, finished)) = export_line {
            div { class: "flex items-start gap-2 border-b border-slate-200 dark:border-slate-800 px-3 py-1.5 text-xs",
                span { class: "min-w-0 break-words font-medium {tone.class()}", "{text}" }
                div { class: "flex-1" }
                if finished {
                    button {
                        class: "shrink-0 rounded px-1.5 py-0.5 text-slate-500 dark:text-slate-400 hover:bg-slate-200 dark:hover:bg-slate-800",
                        title: "Dismiss",
                        onclick: move |_| state.clear_grid_export_status(id),
                        "Dismiss"
                    }
                }
            }
        }
        div { class: "flex items-center gap-2 px-3 py-2 text-sm",
            // Back: return to the view a foreign-key jump came from.
            if can_back {
                button {
                    class: "rounded px-2 py-1 text-xs text-cyan-700 dark:text-cyan-300 hover:bg-cyan-100 dark:hover:bg-cyan-950/40",
                    title: "Back to the previous view",
                    onkeydown: a_held_key_is_not_a_press,
                    onclick: move |_| state.navigate_back(id, Gesture::CLICK),
                    "← Back"
                }
            }
            select {
                class: SELECT,
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
                class: SELECT,
                onchange: move |evt| {
                    filter_op.set(if evt.value() == "equals" { FilterOp::Equals } else { FilterOp::Contains });
                },
                option { value: "contains", "contains" }
                option { value: "equals", "equals" }
            }
            TextInput {
                // `dv-filter` is the target of the `/` focus shortcut.
                id: "dv-filter",
                class: "w-48 font-mono {INPUT}",
                placeholder: "filter value",
                text: "{filter_text}",
                oninput: move |evt: FormEvent| filter_text.set(evt.value()),
                onkeydown: move |evt: KeyboardEvent| {
                    if evt.key() == Key::Enter {
                        query.apply_filter();
                    }
                },
            }
            button {
                class: "rounded bg-slate-300 dark:bg-slate-700 px-3 py-1 text-xs text-slate-900 dark:text-slate-100 hover:bg-slate-400 dark:hover:bg-slate-600",
                onclick: move |_| query.apply_filter(),
                "Apply"
            }
            // An FK jump installs an equality filter the single-column
            // inputs can't show; a chip spells out what's pinned.
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
                    onclick: move |_| query.clear_filter(),
                    "Clear"
                }
            }
            div { class: "flex-1" }
            if let Some(status) = copy_status.read().as_ref() {
                span {
                    class: "min-w-0 truncate text-xs {status.tone().class()}",
                    title: "{status.text}",
                    "{status.text}"
                }
            }

            // Copy-as menu. Copies the selected cells only —
            // the Export buttons beside it cover the whole view.
            if let Some((sel_rows, sel_cols, sel_cells)) = selection_summary {
                div { class: "relative shrink-0",
                    button {
                        class: BTN_GHOST,
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
                    class: BTN_GHOST,
                    title: "Export the current view (filter + sort, all rows) to CSV",
                    onclick: {
                        let export_table = export_table.clone();
                        move |_| spawn_grid_export(state, id, export_table.clone(), export_dialect, sort.peek().clone(), applied_filter.peek().clone(), ExportFormat::Csv)
                    },
                    "Export CSV"
                }
                button {
                    class: BTN_GHOST,
                    title: "Export the current view (filter + sort, all rows) to JSON",
                    onclick: {
                        let export_table = export_table.clone();
                        move |_| spawn_grid_export(state, id, export_table.clone(), export_dialect, sort.peek().clone(), applied_filter.peek().clone(), ExportFormat::Json)
                    },
                    "Export JSON"
                }
            }
            if can_import {
                button {
                    class: "{BTN_GHOST} disabled:opacity-50 disabled:hover:bg-transparent",
                    disabled: import_refused.is_some(),
                    title: match import_refused.as_deref() {
                        Some(reason) => reason.to_string(),
                        None => "Import a CSV or JSON file into this table".to_string(),
                    },
                    onclick: {
                        let import_table = export_table.clone();
                        move |_| pick_import_file(state, id, import_table.clone(), import_file)
                    },
                    "Import…"
                }
            }
            // Row detail: the same toggle as Ctrl+D, here so the
            // panel is discoverable without knowing the shortcut.
            button {
                class: if detail_open {
                    "flex items-center gap-1 rounded bg-slate-300 dark:bg-slate-700 px-2 py-1 text-xs text-slate-900 dark:text-slate-100"
                } else {
                    "flex items-center gap-1 {BTN_GHOST}"
                },
                title: "Show the focused row as a form (Ctrl+D)",
                onclick: move |_| state.set_row_detail(id, !detail_open),
                PanelRight { size: 12 }
                "Row detail"
            }
            button {
                class: "flex items-center gap-1 {BTN_GHOST}",
                title: "Re-run the current query",
                onclick: move |_| state.bump_grid_refresh(id, &refresh_table.key()),
                RefreshCw { size: 12 }
                "Refresh"
            }
        }
        }
    }
}

/// The grid's footer: which rows this page covers, of how many, the aggregate
/// readout for the cell selection, and the page stepper.
///
/// Reads the two resources itself rather than taking their values: the row
/// range and the Next button's bound are derived from both at once, and the
/// derivation is the only thing here worth naming.
///
/// The selection statistics are computed *here*, in a memo, for the same
/// reason `copy_status` is read in the toolbar: read up in [`DataGrid`], every
/// arrow key would re-render the whole grid body. The memo's `PartialEq` gate
/// also means a selection change that leaves the numbers alone (arrowing along
/// a row of equal values) repaints nothing.
#[component]
fn GridFooter(
    /// The page and the row count, read here for the range and the stepper.
    grid: GridPage,
    page: Signal<u64>,
    /// The selection the readout sums, and `copy_status` — where the
    /// click-to-copy confirmation goes: the same line a copy-as reports on,
    /// so there is one place to look for "what just got copied".
    selection: GridSelection,
) -> Element {
    let GridPage {
        rows: rows_resource,
        count: count_resource,
        ..
    } = grid;
    let GridSelection {
        selection,
        grid_nav,
        mut copy_status,
        ..
    } = selection;
    let current = rows_resource.read();
    // Cells already in memory only: `SelectionStats::compute` is a pure
    // function of the page snapshot and the selection rectangle.
    let selection_stats = use_memo(move || {
        let selection = selection()?;
        SelectionStats::for_readout(&grid_nav.read(), selection)
    });
    rsx! {
        div { class: "flex items-center gap-3 border-t border-slate-200 dark:border-slate-800 px-3 py-1.5 text-xs text-slate-500 dark:text-slate-400",
            match current.as_ref() {
                Some(Ok((page_data, _, offset))) => {
                    // The count comes from its own resource; it
                    // stays resolved across page flips, so flipping reuses
                    // it. While it re-runs it reads as unknown, and the
                    // readout says "of …" rather than a stale total.
                    let count_pending = *count_resource.state().read() == UseResourceState::Pending;
                    let total = settled_count(count_pending, count_resource.read().as_ref());
                    let range = row_range(*offset, page_data.result.rows.len() as u64, total);
                    let no_next = !next_page_offered(
                        count_pending,
                        count_resource.read().as_ref(),
                        page(),
                        fetched_rows(current.as_ref()),
                    );
                    rsx! {
                        // Everything left of the stepper lives in one bounded
                        // group: `flex-1 min-w-0 overflow-hidden` fixes its
                        // width at exactly the space the stepper leaves, so a
                        // long readout truncates inside it instead of running
                        // over the buttons. It also replaces the old spacer —
                        // growing to fill is what pushes the stepper right.
                        div { class: "flex min-w-0 flex-1 items-center gap-3 overflow-hidden",
                            span { class: "shrink-0 whitespace-nowrap", "{range}" }
                            // Aggregate readout for the selected cells.
                            // It sits beside the row range deliberately: the range
                            // says what the *page* holds, this says what the
                            // *selection* holds, and the two must never be
                            // confused for one another.
                            if let Some(stats) = selection_stats() {
                            {
                                let line = stats.line();
                                // The clipboard gets the scope note too: a
                                // number pasted elsewhere outlives the tooltip
                                // that qualified it.
                                let clipboard = stats.copy_text();
                                let note = stats.scope_note();
                                let prefix = stats.scope_prefix();
                                let counts = stats.counts_text();
                                let aggregates = stats.aggregate_text();
                                rsx! {
                                    button {
                                        // Drawn in three pieces so the footer
                                        // sheds them in the right order: the
                                        // scope prefix and the aggregates keep
                                        // their width and the counts truncate.
                                        // sum/avg/min/max are what the reader
                                        // came for; "12 cells · 11 non-null" is
                                        // the preamble. The tooltip still
                                        // carries the whole line.
                                        class: "flex min-w-0 items-center gap-1.5 rounded px-1.5 py-0.5 text-left text-slate-600 dark:text-slate-300 hover:bg-slate-200 dark:hover:bg-slate-800 hover:text-slate-900 dark:hover:text-slate-100",
                                        title: "{line}\n\n{note} Click to copy this readout.",
                                        onclick: move |_| {
                                            write_clipboard(&clipboard);
                                            copy_status
                                                .set(
                                                    Some(CopyStatus::ok("Copied the selection stats".to_string())),
                                                );
                                        },
                                        span { class: "shrink-0 whitespace-nowrap", "{prefix}" }
                                        // The counts shed their width first —
                                        // 1000× faster than the aggregates, so
                                        // they are gone before a digit of a sum
                                        // is. The priority is expressed here,
                                        // on the half we are willing to lose,
                                        // rather than as a sub-1 factor on the
                                        // aggregates: a span that cannot shrink
                                        // cannot ellipsise, and gets cut by the
                                        // group's `overflow-hidden` with no
                                        // mark that anything was cut. See
                                        // COUNTS_SHRINK.
                                        span {
                                            class: "min-w-0 truncate",
                                            flex_shrink: COUNTS_SHRINK,
                                            "· {counts}"
                                        }
                                        if let Some(aggregates) = aggregates {
                                            span {
                                                class: "min-w-0 truncate",
                                                flex_shrink: AGGREGATE_SHRINK,
                                                "· {aggregates}"
                                            }
                                        }
                                    }
                                }
                                }
                            }
                        }
                        // The stepper never gives way: the readout beside it
                        // truncates instead, so a long selection summary can't
                        // wrap "← Prev" onto two lines.
                        button {
                            class: "shrink-0 whitespace-nowrap rounded px-2 py-0.5 hover:bg-slate-200 dark:hover:bg-slate-800 disabled:opacity-40",
                            disabled: page() == 0,
                            onclick: move |_| { let p = page(); page.set(p.saturating_sub(1)); },
                            "← Prev"
                        }
                        span { class: "shrink-0 whitespace-nowrap", "page {page() + 1}" }
                        button {
                            class: "shrink-0 whitespace-nowrap rounded px-2 py-0.5 hover:bg-slate-200 dark:hover:bg-slate-800 disabled:opacity-40",
                            disabled: no_next,
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
/// Save/Discard, the write-protection confirmation, and the
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
    confirm: Signal<Option<Armed<Vec<StagedChange>>>>,
    /// Why this table refuses writes, if it does — named in the Save bar so a
    /// disabled Save button is never unexplained.
    read_only_notice: Option<&'static str>,
) -> Element {
    let state = use_context::<AppState>();
    // Read here, not in `DataGrid`: these three are what this chrome is for.
    //
    // A save parked on the write-protection confirmation, and the connection
    // name it has to state. Named rather than just "Are you sure?" — the state
    // exists to make you read which database you are about to change.
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
                    onkeydown: a_held_key_is_not_a_press,
                    // Two-step save when deletes are staged: the first
                    // click arms the exact-count confirmation, the second
                    // (with the stage unchanged, and not straight after the
                    // first) saves. Stages without deletes save immediately.
                    onclick: move |_| {
                        let Some(stage) = state.table_stage(id, &save_table) else { return };
                        if stage.saving {
                            return;
                        }
                        let changes = stage.changes();
                        let press = save_press(
                            stage.delete_count(),
                            confirm.peek().as_ref(),
                            Gesture::CLICK,
                            &changes,
                        );
                        match press {
                            SavePress::Arm => confirm.set(Some(Armed::new(changes))),
                            SavePress::Wait => {}
                            SavePress::Save => {
                                confirm.set(None);
                                state.save_staged(id, &save_table, Gesture::CLICK);
                            }
                        }
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
        // Write-protection confirmation: the connection is
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
                    onkeydown: a_held_key_is_not_a_press,
                    onclick: move |_| state.confirm_pending_save(id, &confirm_save_table, Gesture::CLICK),
                    "Apply to \"{connection_name}\""
                }
                button {
                    class: "rounded border border-slate-400 dark:border-slate-600 px-3 py-1 text-slate-900 dark:text-slate-300 hover:bg-slate-200 dark:hover:bg-slate-800",
                    // Focus on the safe answer, as in every inline
                    // confirmation: Enter pressed at the prompt keeps the
                    // changes staged rather than writing them.
                    onmounted: focus_on_mount,
                    // And a held Enter from Save parks the prompt and stays
                    // parked, rather than parking it and then pressing this.
                    onkeydown: a_held_key_is_not_a_press,
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
                        state.stage_deletes(id, &delete_table, locators);
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

/// Save is blocked while a save is in flight, while any pending insert still
/// lacks a required column, or when the table's resolved capabilities forbid
/// writing. The last case shouldn't arise — changes can only be
/// staged where cells are editable — but a Save button that can only fail is
/// worse than a disabled one carrying the reason.
fn save_disabled(saving: bool, missing_required: usize, can_mutate: bool) -> bool {
    saving || missing_required > 0 || !can_mutate
}

/// The red inline message shown while required insert cells are unfilled.
fn required_missing_message(missing: usize) -> Option<String> {
    (missing > 0).then(|| format!("{missing} required column(s) missing in pending insert(s)"))
}

/// What a Save press does, on the grid's side of the save: the exact-count
/// delete confirmation. (Write protection is the state's, in
/// `save_staged`.)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SavePress {
    /// Arm the confirmation for this change list.
    Arm,
    /// Nothing: the press that armed it, repeated.
    Wait,
    /// Hand the stage to `save_staged`.
    Save,
}

/// Resolves a Save press: `deletes` is how many deletes are staged, `armed`
/// the change list a previous press armed (if any), `gesture` how this press
/// arrived, `current` what is staged now.
///
/// Stages without deletes save at once. With deletes, a press saves only the
/// exact list it armed, and only as a fresh, deliberate press
/// ([`Armed::completes`]): Save relabels itself "Confirm: delete N" in place,
/// so without that a double-click or a held Enter on Save was the
/// confirmation. Such a press waits and stays armed; when to ask is
/// unchanged.
fn save_press(
    deletes: usize,
    armed: Option<&Armed<Vec<StagedChange>>>,
    gesture: Gesture,
    current: &[StagedChange],
) -> SavePress {
    if deletes == 0 {
        return SavePress::Save;
    }
    match armed {
        Some(armed) if armed.subject() == current && armed.completes(gesture) => SavePress::Save,
        Some(armed) if armed.subject() == current => SavePress::Wait,
        _ => SavePress::Arm,
    }
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
    // `spawn_forever`: the save dialog does not block the window, and a
    // table switch meanwhile unmounts this grid. A scoped task was cancelled
    // with it, so the dialog accepted a path and nothing was written or
    // reported — measured. The export needs nothing from the grid once its
    // query is built, and its line names the table wherever it shows
    // ([`grid_export_line`]).
    dioxus::core::spawn_forever(async move {
        let picked = pick_export_path("Export table", &table.name, format).await;
        if let Some(path) = picked {
            state.export_query(id, table, sql, params, format, path);
        }
    });
}

/// A short chip describing an equality (foreign-key) filter, e.g.
/// `artist_id = 1, seq = 2`. `None` for no filter or the single-column filter
/// bar filter (which the inputs already show).
fn equality_filter_label(filter: Option<&Filter>) -> Option<String> {
    match filter? {
        Filter::Equalities(terms) => Some(
            terms
                .iter()
                .map(|term| format!("{} = {}", term.column, term.value.display()))
                .collect::<Vec<_>>()
                .join(", "),
        ),
        Filter::Column { .. } => None,
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
    /// (for copy planning); `identity` decides whether its row can be
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

    /// The top-level item of the grid's shipped code — across every file
    /// under `src/ui/grid`, so a pin follows its code when it moves between
    /// them — whose first line contains `start`, through the line closing
    /// it. Comments are dropped and each line's whitespace collapsed, lines
    /// joined by single spaces: a comment cannot satisfy an assertion and a
    /// rustfmt reflow cannot fail one.
    ///
    /// Exactly one item may match, so a pin cannot quietly read a namesake.
    pub(in crate::ui::grid) fn grid_item(start: &str) -> String {
        use crate::source_scan::{rust_files, shipped, without_comments};
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/ui/grid");
        let files = rust_files(&dir);
        assert!(files.len() > 8, "found only {} grid files", files.len());
        let mut found = Vec::new();
        for file in files {
            let code = without_comments(&shipped(&std::fs::read_to_string(&file).unwrap()));
            let lines: Vec<&str> = code.lines().collect();
            let Some(at) = lines.iter().position(|line| line.contains(start)) else {
                continue;
            };
            let end = lines[at..]
                .iter()
                .position(|line| *line == "}")
                .unwrap_or_else(|| panic!("{}: `{start}` never closes", file.display()));
            found.push(
                lines[at..=at + end]
                    .iter()
                    .map(|line| line.split_whitespace().collect::<Vec<_>>().join(" "))
                    .filter(|line| !line.is_empty())
                    .collect::<Vec<_>>()
                    .join(" "),
            );
        }
        assert_eq!(found.len(), 1, "`{start}` starts {} items", found.len());
        found.remove(0)
    }
}

#[cfg(test)]
mod tests {
    use super::fixtures::*;
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
    fn a_double_click_or_a_held_key_on_save_does_not_confirm_the_deletes() {
        let delete = |id| StagedChange::Delete {
            locator: RowLocator {
                identity_values: vec![Value::Integer(id)],
            },
        };
        let current = vec![delete(1)];
        let settled = Armed::armed_ago(current.clone(), crate::ui::notice::CONFIRM_ARM_DELAY);
        // The first press arms; its repeat inside the delay, or a held key's
        // repeat however late, waits and stays armed; a settled fresh press
        // saves.
        assert_eq!(
            save_press(1, None, Gesture::CLICK, &current),
            SavePress::Arm
        );
        assert_eq!(
            save_press(
                1,
                Some(&Armed::new(current.clone())),
                Gesture::CLICK,
                &current
            ),
            SavePress::Wait
        );
        assert_eq!(
            save_press(1, Some(&settled), Gesture::key(true), &current),
            SavePress::Wait
        );
        assert_eq!(
            save_press(1, Some(&settled), Gesture::CLICK, &current),
            SavePress::Save
        );
        // A confirmation armed for another change list asks again — the
        // exact-count rule is unchanged.
        let other = Armed::armed_ago(vec![delete(2)], crate::ui::notice::CONFIRM_ARM_DELAY);
        assert_eq!(
            save_press(1, Some(&other), Gesture::CLICK, &current),
            SavePress::Arm
        );
        // No deletes, nothing to confirm on this side.
        assert_eq!(save_press(0, None, Gesture::CLICK, &[]), SavePress::Save);
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

    /// A grid stand-in: owns an import-target signal the way [`DataGrid`]
    /// owns `import_file`, and hands it out through the root context.
    #[derive(Clone, Copy)]
    struct Slot(Signal<Option<Signal<Option<std::path::PathBuf>>>>);
    #[derive(Clone, Copy)]
    struct Mounted(Signal<bool>);

    #[component]
    fn TargetOwner() -> Element {
        let own = use_signal(|| None::<std::path::PathBuf>);
        let mut slot = use_context::<Slot>().0;
        use_hook(move || slot.set(Some(own)));
        rsx! {}
    }

    #[component]
    fn MaybeOwner() -> Element {
        let mounted = use_context::<Mounted>().0;
        rsx! {
            if mounted() {
                TargetOwner {}
            }
        }
    }

    /// A mounted [`TargetOwner`] with an [`AppState`] in the root context:
    /// the dom, the state, the owner's signal, and the switch that unmounts it.
    fn mounted_owner() -> (
        dioxus::core::VirtualDom,
        AppState,
        Signal<Option<std::path::PathBuf>>,
        Signal<bool>,
    ) {
        use dioxus::core::{NoOpMutations, VirtualDom};
        let mut dom = VirtualDom::new(MaybeOwner);
        let state = crate::ui::test_dom::provide_state(&dom);
        let (slot, mounted) = dom.in_runtime(|| {
            (
                Slot(Signal::new_in_scope(None, ScopeId::ROOT)),
                Mounted(Signal::new_in_scope(true, ScopeId::ROOT)),
            )
        });
        dom.provide_root_context(slot);
        dom.provide_root_context(mounted);
        dom.rebuild(&mut NoOpMutations);
        let own = dom.in_runtime(|| slot.0.peek().expect("the owner mounted"));
        (dom, state, own, mounted.0)
    }

    fn unmount(dom: &mut dioxus::core::VirtualDom, mut mounted: Signal<bool>) {
        dom.in_runtime(|| mounted.set(false));
        dom.render_immediate(&mut dioxus::core::NoOpMutations);
    }

    fn connection() -> ConnectionId {
        use crate::db::{ConnectionRegistry, DbPool, WriteProtection};
        let pool =
            DbPool::Sqlite(sqlx::sqlite::SqlitePool::connect_lazy("sqlite::memory:").unwrap());
        ConnectionRegistry::default().insert("t.db", pool, WriteProtection::Open)
    }

    fn people() -> TableRef {
        TableRef {
            schema: Some("main".into()),
            name: "people".into(),
        }
    }

    /// The Dioxus behaviour [`deliver_import_pick`] rests on: a signal whose
    /// component has unmounted refuses the write rather than accepting it
    /// into nothing.
    #[tokio::test]
    async fn a_signal_refuses_writes_once_its_component_unmounts() {
        let (mut dom, _state, mut own, mounted) = mounted_owner();
        assert!(
            dom.in_runtime(|| own.try_write().is_ok()),
            "a live signal takes the write"
        );
        unmount(&mut dom, mounted);
        assert!(
            dom.in_runtime(|| own.try_write().is_err()),
            "an unmounted component's signal must refuse the write"
        );
    }

    /// The whole of what happens after the picker returns: a live grid gets
    /// the file and no status line; a gone one gets nothing and the
    /// connection says the import was cancelled, naming the table; and a
    /// running import's line is never replaced by that message.
    #[tokio::test]
    async fn a_picked_file_reaches_a_live_grid_and_a_gone_one_reports_it_cancelled() {
        let id = connection();
        let path = std::path::PathBuf::from("/tmp/people.csv");

        let (dom, state, own, _) = mounted_owner();
        dom.in_runtime(|| {
            deliver_import_pick(state, id, &people(), own, path.clone());
            assert_eq!(*own.peek(), Some(path.clone()));
            assert_eq!(state.import_status.peek().get(&id), None);
        });

        let (mut dom, state, own, mounted) = mounted_owner();
        unmount(&mut dom, mounted);
        dom.in_runtime(|| {
            deliver_import_pick(state, id, &people(), own, path.clone());
            assert_eq!(
                state.import_status.peek().get(&id),
                Some(&ImportStatus::Cancelled {
                    table: "main.people".into()
                })
            );
        });

        let (mut dom, state, own, mounted) = mounted_owner();
        unmount(&mut dom, mounted);
        dom.in_runtime(|| {
            let running = ImportStatus::Running {
                table: "orders".into(),
            };
            let mut statuses = state.import_status;
            statuses.write().insert(id, running.clone());
            deliver_import_pick(state, id, &people(), own, path.clone());
            assert_eq!(state.import_status.peek().get(&id), Some(&running));
        });
    }

    /// The three save/open dialogs must outlive the component that opened
    /// them: each is async and leaves the window usable, so the component can
    /// unmount (a rerun, a table switch) while it is open — and a scoped
    /// `spawn` then took the user's choice and did nothing with it. Read from
    /// the source because the dialogs cannot run in a test; comments are
    /// stripped so a doc mentioning either call cannot satisfy the check.
    #[test]
    fn every_dialog_task_outlives_its_component() {
        let code = |source: &str, function: &str| -> String {
            let start = source
                .find(&format!("fn {function}("))
                .unwrap_or_else(|| panic!("{function} must still exist"));
            let end = start + source[start..].find("\n}\n").expect("its body ends");
            source[start..end]
                .lines()
                .filter(|line| !line.trim_start().starts_with("//"))
                .map(|line| line.split(" //").next().unwrap_or(line))
                .collect::<Vec<_>>()
                .join("\n")
        };
        // The two exports open theirs through the shared `pick_export_path`,
        // which only awaits the dialog — so it is the caller's task that has
        // to outlive the component, and the caller that is checked.
        for (source, function, dialog) in [
            (
                include_str!("mod.rs"),
                "spawn_grid_export",
                "pick_export_path(",
            ),
            (
                include_str!("mod.rs"),
                "pick_import_file",
                "rfd::AsyncFileDialog",
            ),
            (
                include_str!("../editor.rs"),
                "spawn_result_export",
                "pick_export_path(",
            ),
        ] {
            let body = code(source, function);
            assert!(body.contains(dialog), "{function}: {body}");
            assert!(
                body.contains("spawn_forever(async move"),
                "{function}: {body}"
            );
            let scoped = body.replace("spawn_forever(", "");
            assert!(
                !scoped.contains("spawn("),
                "{function} spawns a scoped task: {body}"
            );
        }
        let picker = code(include_str!("../mod.rs"), "pick_export_path");
        assert!(picker.contains("rfd::AsyncFileDialog"), "{picker}");
        assert!(
            !picker.contains("spawn"),
            "the picker spawns a task: {picker}"
        );
    }

    /// The reset after a page, sort, filter or refresh hands focus back when
    /// it took it away (Save's button leaves with the pending bar), and only
    /// reclaims it — it must not take focus from the filter box.
    #[test]
    fn the_reset_effect_reclaims_focus_and_never_takes_it() {
        let reset = grid_item("fn use_grid_reset(");
        assert!(
            reset.contains(r#"reclaim_focus_by_id_next_frame("dv-grid");"#),
            "the reset no longer hands focus back: {reset}"
        );
        // Every focus call in it is a reclaim: none takes focus outright.
        assert_eq!(
            reset.matches("focus_by_id_next_frame").count(),
            reset.matches("reclaim_focus_by_id_next_frame").count(),
            "the reset takes focus outright: {reset}"
        );
    }
}
