//! The grid's body: the rows of the fetched page — windowed to the viewport
//! — with their header, the pending-insert rows under them, and the states
//! shown in their place (loading, an error, an object with nothing to
//! browse, an empty table, a filter matching nothing).
//!
//! [`GridBody`] renders what sits inside `#dv-grid`; [`DataGrid`] keeps the
//! container itself, since its key handler steers the whole grid.

use super::*;

/// Fixed data-row height in CSS pixels. Rows render one truncated
/// line at `text-xs` + `py-1`, so their height is uniform; pinning it lets the
/// windowed renderer compute exact scroll offsets and spacer heights. Measured
/// in the WebKitGTK webview (rows sit on a 33px pitch). The GridRow `<tr>` is
/// held to this height so an open inline editor can't shift the offsets.
pub(super) const ROW_HEIGHT: f64 = 33.0;

/// Rows rendered above and below the viewport. A margin so a fast
/// flick rarely reveals a blank spacer before the range updates, and so small
/// errors from the sticky header offset never uncover an unrendered row.
pub(super) const ROW_OVERSCAN: usize = 12;

/// Scroll/resize listener installed on `#dv-grid`. rAF-coalesces
/// bursts and reports `[scrollTop, clientHeight]` back over the eval channel so
/// the component can derive the visible row range. Installed once per mount.
pub(super) const GRID_SCROLL_JS: &str = r#"
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

/// The grid container's live scroll offset and viewport height, fed by the
/// listener [`use_visible_rows`] installs on `#dv-grid`. The visible row range
/// is derived from these; only rows in that range (plus overscan) are put in
/// the DOM. Created before the rows are fetched, because the reset that
/// returns a new page to the top writes `scroll_top`.
#[derive(Clone, Copy, PartialEq)]
pub(super) struct RowWindow {
    pub(super) scroll_top: Signal<f64>,
    pub(super) viewport_h: Signal<f64>,
}

/// The two signals of a [`RowWindow`]. `viewport_h` seeds non-zero so the
/// first render before the listener reports still windows to a sane range.
pub(super) fn use_row_window() -> RowWindow {
    RowWindow {
        scroll_top: use_signal(|| 0.0f64),
        viewport_h: use_signal(|| 600.0f64),
    }
}

/// Installs the scroll listener feeding `window` and returns the rows to put
/// in the DOM, as a range into `grid_nav`'s fetched rows.
pub(super) fn use_visible_rows(window: RowWindow, grid_nav: Memo<GridNav>) -> Memo<(usize, usize)> {
    let RowWindow {
        mut scroll_top,
        mut viewport_h,
    } = window;
    // Install a scroll/resize listener on the grid container once per mount and
    // pump the offset/height it reports into `scroll_top`/`viewport_h`, which
    // drive the visible-row range. rAF-coalesced so a scroll burst
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

    // The rows to actually put in the DOM: a contiguous window around
    // the viewport. Arrow-keying to an offscreen row is handled by the
    // focus-scroll effect in `use_selection`, which scrolls that row into view (updating
    // `scroll_top`, so this window then includes it) rather than widening the
    // window to the focus — the latter would drag the window back to a
    // now-offscreen focus and defeat windowing after a mouse scroll.
    let visible_range = use_memo(move || {
        let total = grid_nav.read().rows.len();
        compute_visible_range(scroll_top(), viewport_h(), ROW_HEIGHT, total, ROW_OVERSCAN)
    });
    visible_range
}

/// Which designed empty state a zero-row grid page shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum GridEmpty {
    /// The table (matching the active filter, if any) has no rows.
    Table,
    /// A filter is active and nothing matches it — offers "Clear filter".
    NoMatch,
}

/// Selects the grid's empty state: `None` when the page has rows or pending
/// inserts to render, otherwise the no-filter-match state when a filter is
/// applied ([`GridEmpty::NoMatch`]) or the empty-table state
/// ([`GridEmpty::Table`]) when the whole table is empty.
pub(super) fn empty_state(no_visible_rows: bool, has_filter: bool) -> Option<GridEmpty> {
    if !no_visible_rows {
        return None;
    }
    Some(if has_filter {
        GridEmpty::NoMatch
    } else {
        GridEmpty::Table
    })
}

/// The rows of the page, or what stands in for them.
///
/// Its own component, reading the page, the window and the focus itself, so
/// what changes only the rows repaints only the rows. The selection checkbox
/// column and the "+ New row" affordance follow `select_enabled`, the
/// grid's one answer to whether rows may be inserted or deleted here.
#[component]
pub(super) fn GridBody(
    /// What every row renders against: the table, its metadata, the dialect
    /// and the grid's open editor.
    ctx: RowContext,
    grid: GridPage,
    query: GridQuery,
    focus: GridFocus,
    selection: Memo<Option<Selection>>,
    /// The rows to put in the DOM (see [`use_visible_rows`]).
    visible_range: Memo<(usize, usize)>,
    select_enabled: bool,
    /// Rows ticked for deletion (row key → locator).
    selected: Signal<HashMap<String, RowLocator>>,
    /// The table's pending inserts, drawn as phantom rows under the page.
    inserts: Vec<PendingInsert>,
) -> Element {
    let state = use_context::<AppState>();
    let id = ctx.id;
    let row_table = ctx.table.clone();
    let GridPage {
        rows: rows_resource,
        unbrowsable,
        view: page_view,
        nav: grid_nav,
        ..
    } = grid;
    let applied_filter = query.filter.applied;
    let focused_cell = focus.focused_cell;
    let mut selected = selected;
    let current = rows_resource.read();
    let sort_value = (query.sort)();
    // Moves the focus ring to a clicked cell, extending the selection instead
    // of collapsing it when Shift is held.
    let on_select_cell = move |cell: (usize, usize, bool)| focus.select_cell(grid_nav, cell);
    rsx! {
        match current.as_ref() {
            None => rsx! {
                DelayedLoading { label: "Loading…" }
            },
            // An object the backend declares has no rows to show
            // is not a failure to report in red: it is a known
            // property of the object, not something that just went
            // wrong, and there is simply nothing to list. A
            // RisingWave sink stores nothing itself; a SQLite
            // object the engine cannot resolve has rows hubro
            // cannot reach. The sentence is the backend's own, so
            // it explains the object rather than naming the engine.
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
                // the `page_view` memo — this arm no longer
                // re-derives them from the resource, so a focus move
                // or a copy costs nothing here.
                let view = page_view.read();
                let headers = view.headers.clone();
                let selectable = view.selectable.clone();
                let pending_inserts: Vec<PendingInsert> = inserts.clone();
                let all_selected = !selectable.is_empty() && {
                    let sel = selected.read();
                    selectable.iter().all(|(key, _)| sel.contains_key(key))
                };
                let new_row_table = row_table.clone();
                let empty = empty_state(
                    view.rows.is_empty() && pending_inserts.is_empty(),
                    applied_filter.read().is_some(),
                );
                // Windowed rendering: only rows in the visible
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
                // the rest of the page stays in it.
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
                                    onclick: move |_| query.clear_filter(),
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
                                            // The name carries the identity; the
                                            // index only separates a result that
                                            // repeats one.
                                            key: "{col_index}:{header}",
                                            name: header,
                                            sort: sort_value.clone(),
                                            on_sort: move |name: String| query.sort_by(&name),
                                            // Shift-click selects the whole column;
                                            // a plain click keeps sorting.
                                            on_select_column: move |_| {
                                                let rows = grid_nav.peek().rows.len();
                                                if let Some(axis) = Selection::column(col_index, rows) {
                                                    focus.select(axis);
                                                }
                                            },
                                        }
                                    }
                                }
                            }
                            tbody {
                                // Top spacer: the height of the rows
                                // elided above the window.
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
                                        ctx: ctx.clone(),
                                        row,
                                        // The focused column in this row, else None; only the
                                        // two rows whose focus changed re-render on a move.
                                        focused_col: match focused_cell() {
                                            Some((r, c)) if r == index => Some(c),
                                            _ => None,
                                        },
                                        row_index: index,
                                        // The inclusive span of selected columns in this row, or None
                                        // outside the rectangle — so only rows whose span changed re-render.
                                        selected_cols: selection().and_then(|sel| sel.columns_in(index)),
                                        on_select_cell,
                                        select_enabled,
                                        selected,
                                        on_fk_jump: {
                                            let origin = row_table.clone();
                                            move |(fk, row_values, gesture): (ForeignKeyMeta, HashMap<String, Value>, Gesture)| {
                                                state.navigate_fk(id, &fk, &row_values, &origin, applied_filter.peek().clone(), gesture);
                                            }
                                        },
                                    }
                                }
                                // Bottom spacer: the height of the rows
                                // elided below the window.
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
                                for (offset , insert) in pending_inserts.into_iter().enumerate() {
                                    InsertRow {
                                        key: "{insert.row_key()}",
                                        row_index: total_rows + offset,
                                        focused_col: match focused_cell() {
                                            Some((r, c)) if r == total_rows + offset => Some(c),
                                            _ => None,
                                        },
                                        on_select_cell,
                                        ctx: ctx.clone(),
                                        insert,
                                        headers: headers.clone(),
                                        lead_cell: select_enabled,
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
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
