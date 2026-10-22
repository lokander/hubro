//! Keyboard navigation and windowed rendering for the data grid: the move
//! model, the focus arithmetic, and the per-page snapshot the focusable
//! container's key handler reads.
//!
//! Deliberately free of rendering: everything here is a pure function or a
//! plain value, so the whole navigation model is pinned by unit tests rather
//! than by driving a real grid.

use super::*;

/// A move requested by a grid-navigation key, resolved by
/// [`apply_grid_move`] into a new focused cell or a page change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum GridMove {
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
pub(super) fn grid_move_for(code: Code, ctrl: bool) -> Option<GridMove> {
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
pub(super) enum FocusOutcome {
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
pub(super) fn apply_grid_move(
    pos: (usize, usize),
    mv: GridMove,
    rows: usize,
    cols: usize,
) -> FocusOutcome {
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

/// What the grid does with keyboard focus when its editors' open state is
/// (re)read — see [`grid_refocus`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum GridRefocus {
    /// Focus the grid container unconditionally.
    Take,
    /// Focus the grid container only if focus is nowhere else on purpose:
    /// on the page body (the editor's input that held it is gone) or already
    /// inside the grid.
    Reclaim,
    /// Leave focus where it is.
    Leave,
}

/// Whether the grid takes keyboard focus, given whether an editor (the grid's
/// or the detail panel's) was open on the previous read — `None` on the first
/// read, at mount — and whether one is open now.
///
/// Decided on the edge rather than the level: the signals are re-read on
/// every write, and a reset writes `None` over `None` after every page, sort,
/// filter and refresh, none of which may move focus. On mount the grid takes
/// focus outright, so a table opened from the sidebar answers the arrow keys
/// at once; when an editor closes it only reclaims focus, because a click
/// elsewhere (the filter box) is itself what closed the editor.
pub(super) fn grid_refocus(was_open: Option<bool>, open: bool) -> GridRefocus {
    match (was_open, open) {
        (_, true) => GridRefocus::Leave,
        (None, false) => GridRefocus::Take,
        (Some(true), false) => GridRefocus::Reclaim,
        (Some(false), false) => GridRefocus::Leave,
    }
}

/// The half-open range of row indices to render for a scroll position:
/// windowed rendering keeps only these rows in the DOM. `first` is
/// the row at the top of the viewport (`scroll_top / row_height`); the window
/// spans the viewport's worth of rows plus `overscan` on each side, clamped to
/// `[0, total]`. A zero `total` or non-positive `row_height` yields an empty
/// `(0, 0)` range. `end >= start` always holds.
pub(super) fn compute_visible_range(
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

/// Keyboard-navigation snapshot of the visible page: enough per-cell
/// data (row key, column, editability, display text) for the focusable grid
/// container's key handler to move the focus ring and open the editor without
/// threading render-time borrows into the `'static` closure. Built by a memo
/// from the same fetched page + stage the grid renders.
///
/// The pending inserts — the phantom rows under the fetched ones — are here
/// too, but only for the focus ring: arrows and clicks reach them and Enter
/// opens their editor. They are deliberately *not* rows of the selection:
/// copy, the selection statistics and the row detail panel all read `rows`
/// alone, because a phantom row has no values to read — a cell left to the
/// database shows "default", which is not a value — and the panel edits a
/// fetched row by its locator, which a phantom row does not have.
/// [`selection_of`] and [`select_to`] are where that line is drawn.
#[derive(Debug, Default, Clone, PartialEq)]
pub(super) struct GridNav {
    pub(super) headers: Vec<String>,
    pub(super) rows: Vec<GridNavRow>,
    /// The pending inserts, in render order, below `rows`: focus row
    /// `rows.len() + i` is `inserts[i]`.
    pub(super) inserts: Vec<GridNavInsert>,
}

/// A pending insert as the focus ring sees it: the row key its editor opens
/// under ([`PendingInsert::row_key`]) and, per column, whether that cell
/// opens one (blob and generated cells stay "default", as the row renders).
#[derive(Debug, Clone, PartialEq)]
pub(super) struct GridNavInsert {
    pub(super) key: String,
    pub(super) editable: Vec<bool>,
}

#[derive(Debug, Clone, PartialEq)]
pub(super) struct GridNavRow {
    /// Row key ([`RowLocator::key`]) when addressable — needed to open the
    /// editor; `None` rows can only have their value expanded.
    pub(super) key: Option<String>,
    /// The row's locator, used to fetch a truncated cell's full value on
    /// expand.
    pub(super) locator: Option<RowLocator>,
    pub(super) cells: Vec<GridNavCell>,
}

#[derive(Debug, Clone, PartialEq)]
pub(super) struct GridNavCell {
    pub(super) column: String,
    pub(super) editable: bool,
    /// Whether the stage holds an edit for this cell — the row detail panel
    /// tints its fields from the same snapshot the grid tints its
    /// cells from, so the two can't disagree about what is staged.
    pub(super) dirty: bool,
    pub(super) display: String,
    /// The cell's value as fetched. For a cell carrying `preview` this is only
    /// the bounded prefix — a copy must fetch the full value.
    pub(super) value: Value,
    /// Full-value metadata when this cell is a truncated preview; drives both
    /// the expand-on-Enter fetch and the copy's fetch/refusal
    /// decision.
    pub(super) preview: Option<PreviewInfo>,
}

impl GridNavCell {
    /// Whether this cell is a truncated preview — Enter expands it by fetching
    /// the full value rather than showing the in-hand preview.
    pub(super) fn truncated(&self) -> bool {
        self.preview.is_some()
    }
}

impl GridNav {
    pub(super) fn build(
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
        GridNav {
            headers,
            rows,
            inserts: Vec::new(),
        }
    }

    /// This snapshot with `inserts` as its phantom rows, each column's
    /// editability resolved the way [`InsertRow`] renders it.
    pub(super) fn with_inserts(
        mut self,
        inserts: &[PendingInsert],
        column_kinds: &HashMap<String, (EditorKind, bool)>,
    ) -> Self {
        self.inserts = inserts
            .iter()
            .map(|insert| GridNavInsert {
                key: insert.row_key(),
                editable: self
                    .headers
                    .iter()
                    .map(|column| !column_kind(column, column_kinds).0.is_read_only())
                    .collect(),
            })
            .collect();
        self
    }

    /// (fetched rows on the page, columns): the extent of the selection, copy
    /// and the row detail panel. A zero in either means nothing to select.
    pub(super) fn dims(&self) -> (usize, usize) {
        (self.rows.len(), self.headers.len())
    }

    /// (fetched rows plus pending inserts, columns): the extent the focus
    /// ring moves over.
    pub(super) fn focus_dims(&self) -> (usize, usize) {
        (self.rows.len() + self.inserts.len(), self.headers.len())
    }

    /// The pending insert at focus row `row`, if that row is one.
    pub(super) fn insert_at(&self, row: usize) -> Option<&GridNavInsert> {
        row.checked_sub(self.rows.len())
            .and_then(|i| self.inserts.get(i))
    }
}

/// The cell selection for a focus ring at `focus` with `anchor` pinned, on a
/// page of `fetched_rows` fetched rows: `None` while the focus is on a pending
/// insert, which the selection never covers (see [`GridNav`]).
pub(super) fn selection_of(
    focus: Option<(usize, usize)>,
    anchor: Option<(usize, usize)>,
    fetched_rows: usize,
) -> Option<Selection> {
    let focus = focus.filter(|&(row, _)| row < fetched_rows)?;
    Some(Selection {
        anchor: anchor.unwrap_or(focus),
        focus,
    })
}

/// Where a move or click to `next` leaves the selection: `(anchor, focus)`.
/// Shift extends `current` only while both ends stay on fetched rows; a move
/// onto, or from, a pending insert collapses to the one cell instead, so a
/// rectangle never reaches a phantom row.
pub(super) fn select_to(
    current: Selection,
    next: (usize, usize),
    shift: bool,
    fetched_rows: usize,
) -> (Option<(usize, usize)>, (usize, usize)) {
    let on_fetched = |cell: (usize, usize)| cell.0 < fetched_rows;
    if shift && on_fetched(current.anchor) && on_fetched(current.focus) && on_fetched(next) {
        let moved = current.extended_to(next);
        (Some(moved.anchor), moved.focus)
    } else {
        (None, next)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::grid::fixtures::*;

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

    /// The two-row fetched page of `grid_nav_reports_dims_and_cell_editability`
    /// with two pending inserts under it; `title` is a blob column, so an
    /// insert can edit `id` but not `title`.
    fn nav_with_inserts() -> GridNav {
        let kinds: HashMap<String, (EditorKind, bool)> = [
            ("id".to_string(), (EditorKind::Text, false)),
            ("title".to_string(), (EditorKind::Blob, true)),
        ]
        .into_iter()
        .collect();
        let result = two_column_result();
        let rows = view_rows(&result, &[], 0, Some(&pk_identity()), None, true);
        let mut stage = TableStage::default();
        stage.add_insert();
        stage.add_insert();
        GridNav::build(vec!["id".into(), "title".into()], &rows, &kinds)
            .with_inserts(stage.inserts(), &kinds)
    }

    #[test]
    fn pending_inserts_extend_the_focus_ring_but_not_the_selection() {
        let nav = nav_with_inserts();
        assert_eq!(nav.dims(), (2, 2), "the selection's extent: fetched rows");
        assert_eq!(
            nav.focus_dims(),
            (4, 2),
            "the focus ring's: and the inserts"
        );
        assert_eq!(nav.insert_at(1), None, "row 1 is fetched");
        let first = nav.insert_at(2).expect("row 2 is the first insert");
        assert!(first.key.starts_with("insert:"));
        assert_eq!(first.editable, vec![true, false], "a blob stays default");
        assert_ne!(nav.insert_at(3).map(|i| &i.key), Some(&first.key));
        assert_eq!(nav.insert_at(4), None);
        // Down from the last fetched row reaches the first insert, and stops
        // at the last one.
        let (rows, cols) = nav.focus_dims();
        assert_eq!(
            apply_grid_move((1, 0), GridMove::Down, rows, cols),
            FocusOutcome::Cell((2, 0))
        );
        assert_eq!(
            apply_grid_move((3, 0), GridMove::Down, rows, cols),
            FocusOutcome::Cell((3, 0))
        );
    }

    #[test]
    fn no_selection_covers_a_pending_insert() {
        assert_eq!(
            selection_of(Some((1, 1)), Some((0, 0)), 2),
            Some(Selection {
                anchor: (0, 0),
                focus: (1, 1)
            })
        );
        assert_eq!(
            selection_of(Some((1, 1)), None, 2),
            Some(Selection::single((1, 1)))
        );
        assert_eq!(selection_of(Some((2, 0)), None, 2), None, "on an insert");
        assert_eq!(selection_of(None, None, 2), None);
    }

    #[test]
    fn shift_extends_only_between_fetched_rows() {
        let at = |cell| Selection::single(cell);
        // Fetched to fetched: extends, anchored where it started.
        assert_eq!(
            select_to(at((0, 0)), (1, 1), true, 2),
            (Some((0, 0)), (1, 1))
        );
        // Without Shift: collapses.
        assert_eq!(select_to(at((0, 0)), (1, 1), false, 2), (None, (1, 1)));
        // Onto an insert, from an insert, or from a rectangle anchored on
        // one: collapses to the cell instead.
        assert_eq!(select_to(at((1, 0)), (2, 0), true, 2), (None, (2, 0)));
        assert_eq!(select_to(at((2, 0)), (1, 0), true, 2), (None, (1, 0)));
        let reaching = Selection {
            anchor: (2, 0),
            focus: (1, 0),
        };
        assert_eq!(select_to(reaching, (0, 0), true, 2), (None, (0, 0)));
    }

    #[test]
    fn the_grid_takes_focus_on_mount_and_reclaims_it_when_an_editor_closes() {
        assert_eq!(grid_refocus(None, false), GridRefocus::Take, "mount");
        assert_eq!(grid_refocus(None, true), GridRefocus::Leave);
        assert_eq!(
            grid_refocus(Some(true), false),
            GridRefocus::Reclaim,
            "editor closed"
        );
        assert_eq!(
            grid_refocus(Some(false), true),
            GridRefocus::Leave,
            "editor opened"
        );
        assert_eq!(grid_refocus(Some(true), true), GridRefocus::Leave);
        // The reset after a page, sort, filter or refresh writes `None` over
        // `None`: that must not move focus, or Enter in the filter box hands
        // the next keystroke to the grid.
        assert_eq!(grid_refocus(Some(false), false), GridRefocus::Leave);
    }
}
