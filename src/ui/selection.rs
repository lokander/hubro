//! The grid's rectangular cell selection (FRE-110).
//!
//! A selection is two corners — an `anchor` (where it started) and a `focus`
//! (where it currently ends, which is also the cell wearing the keyboard focus
//! ring) — in `(row, column)` coordinates of the *visible page*. Storing the
//! corners rather than a normalized rectangle is what makes extension work
//! the way people expect: shift-arrowing back past the anchor shrinks the
//! selection instead of flipping it, because the anchor never moves.
//!
//! Everything here is pure index arithmetic, no Dioxus types, so the grid can
//! keep the corners in signals and the maths stays unit-testable. Both corners
//! are always inside the page: the grid clamps them with [`Selection::clamped`]
//! whenever the page shrinks under them (a filter change, a delete, a page
//! flip), so nothing downstream can index out of range.
//!
//! Whole-row/whole-column/select-all are not a separate mode — they are just
//! selections spanning an axis ([`Selection::row`], [`Selection::column`],
//! [`Selection::all`]), so extending, copying, and rendering all treat them
//! like any other rectangle.

/// A rectangular cell selection: the `anchor` corner it grew from and the
/// `focus` corner it ends at. A selection always covers at least one cell
/// (`anchor == focus` is a single-cell selection).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Selection {
    pub anchor: (usize, usize),
    pub focus: (usize, usize),
}

/// The normalized bounds of a selection: inclusive on all four sides.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub top: usize,
    pub left: usize,
    pub bottom: usize,
    pub right: usize,
}

impl Selection {
    /// The one-cell selection at `cell`.
    pub fn single(cell: (usize, usize)) -> Self {
        Selection {
            anchor: cell,
            focus: cell,
        }
    }

    /// The whole of row `row` across a page `cols` wide (`None` for a page
    /// with no columns).
    pub fn row(row: usize, cols: usize) -> Option<Self> {
        // `then`, not `then_some`: the corner arithmetic underflows for a
        // zero-size page and would be evaluated eagerly.
        (cols > 0).then(|| Selection {
            anchor: (row, 0),
            focus: (row, cols - 1),
        })
    }

    /// The whole of column `col` down a page `rows` tall (`None` for an empty
    /// page). The focus lands on the last row so a following shift-arrow
    /// extends from the bottom, as in a spreadsheet.
    pub fn column(col: usize, rows: usize) -> Option<Self> {
        (rows > 0).then(|| Selection {
            anchor: (0, col),
            focus: (rows - 1, col),
        })
    }

    /// The whole page (`None` when it has no cells).
    pub fn all(rows: usize, cols: usize) -> Option<Self> {
        (rows > 0 && cols > 0).then(|| Selection {
            anchor: (0, 0),
            focus: (rows - 1, cols - 1),
        })
    }

    /// The same selection with its focus moved to `cell` — shift-click and
    /// shift-arrow. The anchor is untouched, so the rectangle can shrink back
    /// through it and out the other side.
    pub fn extended_to(self, cell: (usize, usize)) -> Self {
        Selection {
            anchor: self.anchor,
            focus: cell,
        }
    }

    /// Inclusive bounds, with the corners sorted on each axis independently —
    /// so an anchor below/right of the focus describes the same rectangle as
    /// the other way round.
    pub fn bounds(&self) -> Rect {
        Rect {
            top: self.anchor.0.min(self.focus.0),
            left: self.anchor.1.min(self.focus.1),
            bottom: self.anchor.0.max(self.focus.0),
            right: self.anchor.1.max(self.focus.1),
        }
    }

    /// `(rows, columns)` covered.
    pub fn size(&self) -> (usize, usize) {
        let rect = self.bounds();
        (rect.bottom - rect.top + 1, rect.right - rect.left + 1)
    }

    /// Total cells covered (always ≥ 1).
    pub fn cell_count(&self) -> usize {
        let (rows, cols) = self.size();
        rows * cols
    }

    /// Whether this selection is exactly one cell — the case where the plain
    /// copy shortcut copies the raw value instead of a TSV block.
    pub fn is_single(&self) -> bool {
        self.anchor == self.focus
    }

    /// The inclusive column range selected in `row`, or `None` when that row
    /// is outside the selection — membership and rendering both go through
    /// this, so a row renders its selected span without re-deriving the
    /// rectangle per cell (and only rows whose span changed re-render).
    pub fn columns_in(&self, row: usize) -> Option<(usize, usize)> {
        let rect = self.bounds();
        (rect.top..=rect.bottom)
            .contains(&row)
            .then_some((rect.left, rect.right))
    }

    /// Both corners clamped into a `rows`×`cols` page, or `None` when the page
    /// has no cells at all. Called whenever the page changes shape so a stale
    /// selection can never index out of range.
    pub fn clamped(self, rows: usize, cols: usize) -> Option<Self> {
        if rows == 0 || cols == 0 {
            return None;
        }
        let clamp = |(r, c): (usize, usize)| (r.min(rows - 1), c.min(cols - 1));
        Some(Selection {
            anchor: clamp(self.anchor),
            focus: clamp(self.focus),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_single_cell_selection_is_one_cell() {
        let sel = Selection::single((2, 3));
        assert!(sel.is_single());
        assert_eq!(sel.cell_count(), 1);
        assert_eq!(sel.size(), (1, 1));
        assert_eq!(
            sel.bounds(),
            Rect {
                top: 2,
                left: 3,
                bottom: 2,
                right: 3
            }
        );
        assert_eq!(sel.columns_in(2), Some((3, 3)));
        assert_eq!(sel.columns_in(1), None);
    }

    #[test]
    fn bounds_normalize_the_anchor_after_the_focus_on_both_axes() {
        // Anchor bottom-right, focus top-left: the same rectangle.
        let forward = Selection {
            anchor: (1, 2),
            focus: (4, 5),
        };
        let backward = Selection {
            anchor: (4, 5),
            focus: (1, 2),
        };
        let expected = Rect {
            top: 1,
            left: 2,
            bottom: 4,
            right: 5,
        };
        assert_eq!(forward.bounds(), expected);
        assert_eq!(backward.bounds(), expected);
        assert_eq!(forward.cell_count(), backward.cell_count());
        assert_eq!(forward.cell_count(), 4 * 4);

        // Mixed: anchor above but to the right of the focus.
        let mixed = Selection {
            anchor: (1, 5),
            focus: (4, 2),
        };
        assert_eq!(mixed.bounds(), expected);
        // …and the other mixed diagonal.
        let mixed = Selection {
            anchor: (4, 2),
            focus: (1, 5),
        };
        assert_eq!(mixed.bounds(), expected);
    }

    #[test]
    fn extending_keeps_the_anchor_so_the_rectangle_can_shrink_back_through_it() {
        let sel = Selection::single((3, 3));
        let grown = sel.extended_to((5, 5));
        assert_eq!(grown.size(), (3, 3));
        // Back to the anchor: one cell again, not a flipped rectangle.
        let shrunk = grown.extended_to((3, 3));
        assert!(shrunk.is_single());
        // Past the anchor: grows the other way, anchor still fixed.
        let past = grown.extended_to((1, 2));
        assert_eq!(past.anchor, (3, 3));
        assert_eq!(
            past.bounds(),
            Rect {
                top: 1,
                left: 2,
                bottom: 3,
                right: 3
            }
        );
    }

    #[test]
    fn whole_row_and_column_selections_span_their_axis() {
        let row = Selection::row(2, 4).unwrap();
        assert_eq!(row.size(), (1, 4));
        assert_eq!(row.columns_in(2), Some((0, 3)));
        assert_eq!(row.columns_in(3), None);
        assert!(!row.is_single());

        let col = Selection::column(1, 5).unwrap();
        assert_eq!(col.size(), (5, 1));
        assert_eq!(col.focus, (4, 1), "focus at the bottom for shift-arrow");
        assert_eq!(col.columns_in(0), Some((1, 1)));
        assert_eq!(col.columns_in(4), Some((1, 1)));
        assert_eq!(col.columns_in(5), None);

        // A one-column page: a whole row is still a single cell.
        assert!(Selection::row(0, 1).unwrap().is_single());
        // …and a one-row page's whole column likewise.
        assert!(Selection::column(0, 1).unwrap().is_single());
    }

    #[test]
    fn select_all_covers_the_page_and_empty_pages_have_no_selection() {
        let all = Selection::all(3, 4).unwrap();
        assert_eq!(all.cell_count(), 12);
        assert_eq!(all.anchor, (0, 0));
        assert_eq!(all.focus, (2, 3));
        assert_eq!(all.columns_in(2), Some((0, 3)));

        assert_eq!(Selection::all(0, 4), None);
        assert_eq!(Selection::all(3, 0), None);
        assert_eq!(Selection::row(0, 0), None);
        assert_eq!(Selection::column(0, 0), None);
    }

    #[test]
    fn clamping_pulls_a_stale_selection_back_inside_a_shrunken_page() {
        let sel = Selection {
            anchor: (9, 9),
            focus: (0, 0),
        };
        let clamped = sel.clamped(3, 2).unwrap();
        assert_eq!(clamped.anchor, (2, 1));
        assert_eq!(clamped.focus, (0, 0));
        // An in-range selection is untouched.
        let inside = Selection {
            anchor: (1, 1),
            focus: (2, 0),
        };
        assert_eq!(inside.clamped(3, 2), Some(inside));
        // No cells to select at all.
        assert_eq!(inside.clamped(0, 2), None);
        assert_eq!(inside.clamped(3, 0), None);
    }

    #[test]
    fn membership_matches_the_bounds_over_a_whole_page() {
        // Cross-check `columns_in` against an independent membership test for
        // every cell of a small page, for a selection on each diagonal — the
        // rendering path decides a cell's tint through it, so an off-by-one
        // here would paint the wrong block.
        for sel in [
            Selection {
                anchor: (1, 1),
                focus: (2, 3),
            },
            Selection {
                anchor: (2, 3),
                focus: (1, 1),
            },
            Selection {
                anchor: (1, 3),
                focus: (2, 1),
            },
        ] {
            let mut counted = 0;
            for row in 0..4 {
                for col in 0..5 {
                    let inside = (1..=2).contains(&row) && (1..=3).contains(&col);
                    counted += usize::from(inside);
                    assert_eq!(
                        sel.columns_in(row)
                            .is_some_and(|(left, right)| (left..=right).contains(&col)),
                        inside,
                        "cell ({row}, {col})"
                    );
                }
            }
            assert_eq!(sel.cell_count(), counted);
        }
    }
}
