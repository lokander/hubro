//! Who holds the keyboard: the focus ring and the cell selection anchored on
//! it, the two in-place editors that can take the keyboard from it, and the
//! grid's key handler.
//!
//! The rules are [`nav`]'s — moves, selection extents, when to hand focus
//! back — as plain functions over plain data. This file is the hooks and
//! handlers that hold the state those rules act on and apply their answers.

use super::*;

/// The two in-place editors: the grid's and the row detail panel's. At most
/// one of them is open (see [`use_grid_editors`]).
#[derive(Clone, Copy, PartialEq)]
pub(super) struct GridEditors {
    /// Which cell's editor is open in the grid (by row key + column).
    pub(super) editing: Signal<Option<ActiveEdit>>,
    /// The row detail panel's open editor. Lives on the grid, above the
    /// panel, because `RowDetailFields` is keyed by row: anything held inside
    /// it is destroyed by every row move, which silently discarded whatever
    /// the user had typed. Hoisting it is what lets a half-typed draft
    /// survive.
    pub(super) detail_editing: Signal<Option<ActiveEdit>>,
}

/// The two editors' signals, and the rule that keeps one of them open at most.
pub(super) fn use_grid_editors() -> GridEditors {
    let editing = use_signal(|| Option::<ActiveEdit>::None);
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
    GridEditors {
        editing,
        detail_editing,
    }
}

/// The keyboard focus ring and the cell selection anchored on it, plus the
/// copy status every move clears.
#[derive(Clone, Copy, PartialEq)]
pub(super) struct GridFocus {
    /// Keyboard focus ring in the grid (row, col into the visible page's
    /// rows × columns — the pending inserts' rows after the fetched ones).
    pub(super) focused_cell: Signal<Option<(usize, usize)>>,
    /// The other corner of a rectangular cell selection; the focus ring is
    /// the near corner. `None` means the selection is exactly the focused
    /// cell — the common case, and why this is a separate signal rather than
    /// a rewrite of `focused_cell`: every focus behaviour (clamping,
    /// scroll-into-view, Enter) works on the focus alone.
    pub(super) selection_anchor: Signal<Option<(usize, usize)>>,
    /// Outcome of the most recent copy, shown in the toolbar until the next
    /// copy or a page/selection reset. Also carries a refusal.
    pub(super) copy_status: Signal<Option<CopyStatus>>,
}

/// The [`GridFocus`] signals, all empty.
pub(super) fn use_grid_focus() -> GridFocus {
    GridFocus {
        focused_cell: use_signal(|| None),
        selection_anchor: use_signal(|| None),
        copy_status: use_signal(|| None),
    }
}

impl GridFocus {
    /// Moves the focus ring to a clicked cell, extending the selection
    /// instead of collapsing it when Shift is held. A run of shift-clicks all
    /// extend from the same corner, because the anchor never moves.
    pub(super) fn select_cell(
        self,
        grid_nav: Memo<GridNav>,
        (row, col, shift): (usize, usize, bool),
    ) {
        let GridFocus {
            mut focused_cell,
            mut selection_anchor,
            mut copy_status,
        } = self;
        let focus = focused_cell.peek().unwrap_or((row, col));
        let current = Selection {
            anchor: selection_anchor.peek().unwrap_or(focus),
            focus,
        };
        let (anchor, focus) = select_to(current, (row, col), shift, grid_nav.peek().rows.len());
        selection_anchor.set(anchor);
        focused_cell.set(Some(focus));
        copy_status.set(None);
    }

    /// Selects a whole page, row or column: `axis` is its rectangle, focus
    /// corner last.
    pub(super) fn select(self, axis: Selection) {
        let GridFocus {
            mut focused_cell,
            mut selection_anchor,
            mut copy_status,
        } = self;
        selection_anchor.set(Some(axis.anchor));
        focused_cell.set(Some(axis.focus));
        copy_status.set(None);
    }

    /// The row detail panel's Prev/Next: moves the GRID's focus, through the
    /// same resolution an arrow key takes — the panel steers the one
    /// selection rather than keeping a second row of its own.
    pub(super) fn step_row(self, grid_nav: Memo<GridNav>, step: RowStep) {
        let GridFocus {
            mut focused_cell,
            mut selection_anchor,
            mut copy_status,
        } = self;
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
    }
}

/// What the chrome around the rows reads of the selection: the toolbar's
/// copy-as menu and the footer's aggregate readout.
#[derive(Clone, Copy, PartialEq)]
pub(super) struct GridSelection {
    /// The live cell selection (see [`use_selection`]).
    pub(super) selection: Memo<Option<Selection>>,
    /// The page the selection indexes into.
    pub(super) grid_nav: Memo<GridNav>,
    /// Where a copy reports, and whether the copy-as menu is open.
    pub(super) copy_status: Signal<Option<CopyStatus>>,
    pub(super) copy_menu: Signal<bool>,
}

/// Keeps `focus` on the page `grid_nav` describes and in view, and derives
/// the live selection from it.
pub(super) fn use_selection(focus: GridFocus, grid_nav: Memo<GridNav>) -> Memo<Option<Selection>> {
    let GridFocus {
        mut focused_cell,
        mut selection_anchor,
        ..
    } = focus;
    // Keep the focus ring inside the current page and seed it once data
    // arrives, so it is visible and never indexes out of range after a page or
    // filter change shrinks the grid.
    // The selection's far corner is clamped alongside the focus: a
    // rectangle reaching past a page that just shrank would address rows the
    // grid no longer has.
    use_effect(move || {
        let (rows, cols) = grid_nav.read().focus_dims();
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

    // The live cell selection: the focus ring's cell plus the
    // anchor, when one is pinned. `None` only for a page with nothing to
    // select. Derived rather than stored so the two corners can never drift
    // apart from the focus the rest of the grid navigates by.
    //
    // `None` too while the focus ring is on a pending insert, which no
    // selection covers (see `GridNav`).
    let selection = use_memo(move || {
        selection_of(
            *focused_cell.read(),
            *selection_anchor.read(),
            grid_nav.read().rows.len(),
        )
    });

    // Scroll the focused cell into view as it moves. With windowed rows an
    // offscreen focused row may not be in the DOM yet, so we can't rely on
    // its node: instead scroll the container to the focused
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
    selection
}

/// Gives keyboard focus to the grid container on mount, and hands it back
/// after an editor closes.
pub(super) fn use_editor_refocus(editors: GridEditors) {
    let GridEditors {
        editing,
        detail_editing,
    } = editors;
    // Give keyboard focus to the grid container on mount, and hand it back
    // after an editor closes (the editor input, not the container, held
    // focus), so arrow navigation keeps working without a mouse click.
    //
    // Both editors count: watching only the grid's left the arrow
    // keys dead after a commit in the panel, because focus stayed on the
    // closed panel editor's input and nothing handed it back.
    //
    // `grid_refocus` decides from the *edge*, not the level: this effect
    // re-runs on every write to either signal, and a write of `None` over
    // `None` (any page, sort, filter or refresh reset) used to pull focus
    // out of the filter box the user had just pressed Enter in. And handing
    // focus back only reclaims it — it leaves alone a focus the user moved
    // elsewhere, as clicking the filter box while an editor was open does
    // (the click's blur is what commits and closes the editor).
    let mut editor_was_open = use_hook(|| CopyValue::new(None::<bool>));
    use_effect(move || {
        let open = editing.read().is_some() || detail_editing.read().is_some();
        let was_open = *editor_was_open.peek();
        editor_was_open.set(Some(open));
        match grid_refocus(was_open, open) {
            GridRefocus::Take => focus_by_id_next_frame("dv-grid"),
            GridRefocus::Reclaim => reclaim_focus_by_id_next_frame("dv-grid"),
            GridRefocus::Leave => {}
        }
    });
}

/// What Enter does on the focused cell.
#[derive(Debug, Clone, PartialEq)]
enum EnterAction {
    /// Open the in-place editor on this cell.
    Edit(ActiveEdit),
    /// Show the cell's full value in the popup.
    Expand(ExpandView),
    /// Nothing: an empty page, or a pending insert's locked cell (a blob or
    /// generated column, which stays "default" until saved).
    Nothing,
}

/// Resolves Enter at focus `pos`, which may be on a fetched row or a pending
/// insert: an editable cell opens its editor — a pending insert's under its
/// own row key, the one [`InsertCellSlot`] opens for — and a fetched cell that
/// can't be edited expands instead. A phantom row has nothing to expand.
fn enter_action(nav: &GridNav, pos: (usize, usize)) -> EnterAction {
    let (rows, cols) = nav.focus_dims();
    if rows == 0 || cols == 0 {
        return EnterAction::Nothing;
    }
    let (r, c) = (pos.0.min(rows - 1), pos.1.min(cols - 1));
    let edit = |row_key: String| {
        EnterAction::Edit(ActiveEdit {
            row_key,
            column: nav.headers[c].clone(),
            draft: None,
        })
    };
    if let Some(insert) = nav.insert_at(r) {
        return match insert.editable.get(c) {
            Some(true) => edit(insert.key.clone()),
            _ => EnterAction::Nothing,
        };
    }
    let Some(row) = nav.rows.get(r) else {
        return EnterAction::Nothing;
    };
    let Some(cell) = row.cells.get(c) else {
        return EnterAction::Nothing;
    };
    match (cell.editable, row.key.clone()) {
        (true, Some(key)) => edit(key),
        _ => EnterAction::Expand(expand_view(cell, row.locator.clone())),
    }
}

/// The grid's keyboard handler. Attached to the focusable scroll container,
/// so it also receives keydowns bubbling from focused children; it no-ops
/// while an editor is open (whose own keys bubble here). Movement and Enter
/// act on `grid_nav`/`focused_cell`; PageUp/PageDown flip pages. Selection
/// and copy ride on the same model: Shift extends instead of collapsing,
/// Ctrl+A / Shift+Space / Ctrl+Space select the page / row / column, and
/// Ctrl+C copies.
pub(super) fn grid_key_handler(
    editing: Signal<Option<ActiveEdit>>,
    expanded: Signal<Option<ExpandView>>,
    focus: GridFocus,
    copy_menu: Signal<bool>,
    key_copy_ctx: CopyContext,
    grid: GridPage,
    page: Signal<u64>,
) -> impl FnMut(KeyboardEvent) + 'static {
    let mut editing = editing;
    let mut expanded = expanded;
    let mut copy_menu = copy_menu;
    let mut page = page;
    let GridFocus {
        mut focused_cell,
        mut selection_anchor,
        mut copy_status,
    } = focus;
    let GridPage {
        rows: rows_resource,
        count: count_resource,
        nav: grid_nav,
        ..
    } = grid;
    move |evt: KeyboardEvent| {
        if editing.peek().is_some() {
            return;
        }
        let code = evt.code();
        // Escape closes the value-expand popup, if one is open — and
        // otherwise collapses a multi-cell selection back to the focus.
        if code == Code::Escape {
            if expanded.peek().is_some() {
                evt.prevent_default();
                close_expanded(expanded);
            } else if selection_anchor.peek().is_some() {
                evt.prevent_default();
                selection_anchor.set(None);
            }
            return;
        }
        let modifiers = evt.modifiers();
        let ctrl = accel_held(modifiers);
        let shift = modifiers.shift();
        // Two extents: the focus ring moves over the pending inserts too,
        // while a selection — and so copying, and selecting a whole row or
        // page — covers the fetched rows only.
        let (focus_rows, cols) = grid_nav.peek().focus_dims();
        let rows = grid_nav.peek().rows.len();
        if focus_rows == 0 || cols == 0 {
            return;
        }
        let pos = focused_cell.peek().unwrap_or((0, 0));
        let current = Selection {
            anchor: selection_anchor.peek().unwrap_or(pos),
            focus: pos,
        };
        let on_insert = pos.0 >= rows;
        // Copy: the plain shortcut, so `None` — a single cell
        // copies its raw value, a block copies TSV. A pending insert has no
        // value to copy.
        if ctrl && code == Code::KeyC {
            evt.prevent_default();
            if !on_insert {
                copy_menu.set(false);
                start_copy(&key_copy_ctx, &grid_nav.peek(), current, None);
            }
            return;
        }
        // Select all / whole row / whole column. Ctrl+A must also suppress
        // the webview's own "select the page's text".
        let axis = match code {
            Code::KeyA if ctrl => Selection::all(rows, cols),
            Code::Space if ctrl => Selection::column(pos.1, rows),
            Code::Space if shift && !on_insert => Selection::row(pos.0, cols),
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
            // Editable cell → open the in-place editor; otherwise expand
            // the value. A truncated cell fetches its full value on demand;
            // a complete one shows the in-hand text.
            let action = enter_action(&grid_nav.peek(), pos);
            match action {
                EnterAction::Edit(edit) => editing.set(Some(edit)),
                EnterAction::Expand(view) => expanded.set(Some(view)),
                EnterAction::Nothing => {}
            }
            return;
        }
        let Some(mv) = grid_move_for(code, ctrl) else {
            return;
        };
        evt.prevent_default();
        match apply_grid_move(pos, mv, focus_rows, cols) {
            FocusOutcome::Cell(next) => {
                // Shift extends the rectangle (the anchor stays where the
                // selection started); an unmodified move collapses it.
                let (anchor, focus) = select_to(current, next, shift, rows);
                selection_anchor.set(anchor);
                focused_cell.set(Some(focus));
            }
            FocusOutcome::PrevPage => {
                let p = *page.peek();
                if p > 0 {
                    page.set(p - 1);
                }
            }
            FocusOutcome::NextPage => {
                // The Next button's rule, so the two cannot disagree.
                let p = *page.peek();
                let offered = next_page_offered(
                    *count_resource.state().peek() == UseResourceState::Pending,
                    count_resource.peek().as_ref(),
                    p,
                    fetched_rows(rows_resource.peek().as_ref()),
                );
                if offered {
                    page.set(p + 1);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::grid::fixtures::*;

    #[test]
    fn enter_opens_a_pending_inserts_own_editor_and_never_expands_it() {
        let kinds: HashMap<String, (EditorKind, bool)> = [
            ("id".to_string(), (EditorKind::Text, false)),
            ("title".to_string(), (EditorKind::Text, true)),
        ]
        .into_iter()
        .collect();
        let result = two_column_result();
        let rows = view_rows(&result, &[], 0, Some(&pk_identity()), None, true);
        let mut stage = TableStage::default();
        let insert = stage.add_insert();
        let nav = GridNav::build(vec!["id".into(), "title".into()], &rows, &kinds)
            .with_inserts(stage.inserts(), &kinds);

        // Two fetched rows, then the insert at focus row 2.
        // The insert's cell: its editor, under the key `InsertCellSlot`
        // matches on.
        assert_eq!(
            enter_action(&nav, (2, 1)),
            EnterAction::Edit(ActiveEdit {
                row_key: format!("insert:{insert}"),
                column: "title".into(),
                draft: None,
            })
        );
        // A fetched cell is unchanged: its own row key.
        let EnterAction::Edit(edit) = enter_action(&nav, (0, 1)) else {
            panic!("a fetched editable cell opens its editor");
        };
        assert_eq!(edit.row_key, nav.rows[0].key.clone().unwrap());
        // A stale focus past the inserts clamps onto the last one.
        assert!(
            matches!(enter_action(&nav, (9, 1)), EnterAction::Edit(e) if e.row_key.starts_with("insert:"))
        );
        // An empty page: nothing.
        assert_eq!(
            enter_action(&GridNav::default(), (0, 0)),
            EnterAction::Nothing
        );
    }

    #[test]
    fn enter_on_a_pending_inserts_locked_cell_does_nothing() {
        let mut nav = GridNav::build(vec!["id".into(), "cover".into()], &[], &HashMap::new());
        nav.inserts = vec![GridNavInsert {
            key: "insert:7".into(),
            editable: vec![true, false],
        }];
        assert_eq!(enter_action(&nav, (0, 1)), EnterAction::Nothing);
        assert!(matches!(enter_action(&nav, (0, 0)), EnterAction::Edit(_)));
    }

    /// `grid_refocus` is pinned by its own test; this pins that the focus
    /// effect acts on its answer as written — each arm its own focus call,
    /// and the edge remembered — which a driven run cannot repeat.
    #[test]
    fn the_focus_effect_does_what_grid_refocus_says() {
        let effect = grid_item("fn use_editor_refocus(");
        for wire in [
            "let open = editing.read().is_some() || detail_editing.read().is_some();",
            "let was_open = *editor_was_open.peek();",
            "editor_was_open.set(Some(open));",
            "match grid_refocus(was_open, open) {",
            r#"GridRefocus::Take => focus_by_id_next_frame("dv-grid"),"#,
            r#"GridRefocus::Reclaim => reclaim_focus_by_id_next_frame("dv-grid"),"#,
            "GridRefocus::Leave => {}",
        ] {
            assert!(
                effect.contains(wire),
                "the focus effect lost `{wire}`: {effect}"
            );
        }
    }

    /// The grid's shortcuts follow the app's one Ctrl rule, `accel_held` —
    /// the window-level half is `every_window_shortcut_takes_ctrl_or_cmd`.
    #[test]
    fn the_grid_reads_ctrl_through_the_shared_rule() {
        let handler = grid_item("fn grid_key_handler(");
        assert!(
            handler.contains("let ctrl = accel_held(modifiers);"),
            "the grid's Ctrl is not `accel_held`: {handler}"
        );
        for direct in [".ctrl()", ".meta()"] {
            assert!(
                !handler.contains(direct),
                "the grid reads `{direct}` directly instead of through `accel_held`"
            );
        }
    }
}
