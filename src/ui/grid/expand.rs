//! The value-expand popup: Enter on a cell that can't be edited in place
//! shows its whole value — in hand when the page holds all of it, fetched on
//! demand when the page holds a preview — through the rich cell viewers.

use super::*;
use crate::ui::classes::BTN_SECONDARY_SM;

/// What the value-expand popup shows: a short value already in hand, or a
/// truncated cell whose full value is loaded lazily via
/// [`AppState::load_cell`].
#[derive(Debug, Clone, PartialEq)]
pub(super) enum ExpandView {
    /// A value already fully in the page — rendered directly, and safe to
    /// copy.
    ///
    /// Carries the [`Value`], not its display string: the popup renders it
    /// through [`CellViewer`], and "Copy raw" has to put the same
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
    /// [`plan_copy`] makes for this cell. Copying here would put a
    /// prefix plus a literal `…` on the clipboard.
    Truncated { display: String, column: String },
    /// A truncated cell: fetch and show its full value.
    Fetch { locator: RowLocator, column: String },
}

/// What Enter on a non-editable focused cell expands to: the value already in
/// hand, or — for a truncated preview — a fetch of the full value,
/// which needs the row to be addressable.
pub(super) fn expand_view(cell: &GridNavCell, locator: Option<RowLocator>) -> ExpandView {
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

/// Closes the value-expand popup and hands focus back to the grid: the
/// popup held it, and it is about to be removed.
pub(super) fn close_expanded(mut expanded: Signal<Option<ExpandView>>) {
    expanded.set(None);
    focus_by_id_next_frame("dv-grid");
}

/// The value-expand popup, while `expanded` holds a value to show. Dismissed
/// by a backdrop click, the ✕, or Escape.
///
/// The popup owns its Escape, like every `Overlay`: it renders beside
/// `#dv-grid`, not inside it, so once a click on its text or its Copy raw
/// button has taken focus the grid's handler never sees the key. The backdrop
/// takes focus when it opens and holds it for any click inside that lands on
/// nothing focusable; closing hands focus back to the grid, since the element
/// that had it is gone.
#[component]
pub(super) fn ExpandPopup(
    id: ConnectionId,
    table: TableRef,
    /// The table's render metadata, for the declared type each viewer
    /// classifies on.
    meta: Shared<TableRenderMeta>,
    /// What the popup shows; `None` while it is closed. Written by the grid's
    /// Enter, cleared here.
    expanded: Signal<Option<ExpandView>>,
) -> Element {
    rsx! {
        if let Some(view) = expanded.read().clone() {
            Overlay {
                on_close: move |_| close_expanded(expanded),
                dismiss_on_backdrop: true,
                align: OverlayAlign::Center,
                div {
                    class: "max-h-[70vh] w-full max-w-2xl overflow-auto rounded-lg border border-slate-300 dark:border-slate-700 bg-white dark:bg-slate-900 p-4 shadow-xl",
                    div { class: "mb-2 flex items-center justify-between",
                        span { class: "text-xs font-semibold uppercase tracking-wide text-slate-500 dark:text-slate-400",
                            "Cell value"
                        }
                        button {
                            class: "rounded px-2 py-0.5 text-sm text-slate-500 dark:text-slate-400 hover:bg-slate-200 dark:hover:bg-slate-800 hover:text-slate-900 dark:hover:text-slate-100",
                            aria_label: "Close",
                            onclick: move |_| close_expanded(expanded),
                            X { size: 16 }
                        }
                    }
                    match view {
                        // A short value already in hand is always the full
                        // value, so the rich viewers may decode it: JSON
                        // as a tree, an image as a picture.
                        ExpandView::Text { value, column } => {
                            let type_name = meta.type_of(&column);
                            rsx! {
                                CopyRawButton { value: value.clone() }
                                CellViewer { value, type_name, truncated: None }
                            }
                        },
                        // Preview of an unloadable value: readable, but no
                        // Copy raw — it would put a prefix on the clipboard.
                        // Not pretty-printed either; the text is
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

/// The full-value body of the expand popup for a truncated cell:
/// loads the value via [`AppState::load_cell`] and hands it to [`CellViewer`]
/// — a JSON tree, a picture, a hex dump or a text pane. A value over
/// the fetch cap arrives as a prefix, and the viewer is told so, which is what
/// keeps it from decoding half a document or half an image as a whole one.
#[component]
pub(super) fn ExpandedValue(
    id: ConnectionId,
    table: TableRef,
    locator: RowLocator,
    column: String,
    /// The column's declared type, looked up by the popup (empty when the
    /// schema doesn't name it) — see [`TableRenderMeta::type_of`].
    type_name: String,
) -> Element {
    let refused_column = column.clone();
    rsx! {
        LoadedCell {
            id,
            table,
            locator,
            column,
            loaded: move |fetch: CellFetch| {
                let capped = fetch.capped;
                // A capped fetch holds a prefix, so Copy raw is withdrawn rather
                // than handed a truncated value — the same refusal `plan_copy`
                // makes for this cell, worded identically. What is on
                // screen is the viewer's to describe; this says only what was
                // loaded and what that costs.
                let capped_note = capped.then(|| {
                    format!(
                        "Value is very large; only the first {} was loaded. {}",
                        human_bytes(FETCH_CELL_MAX_BYTES as u64),
                        CopyRefusal::TooLarge {
                            column: refused_column.clone(),
                            full_len: fetch.full_len,
                        }
                        .message(),
                    )
                });
                rsx! {
                    if let Some(note) = capped_note {
                        Banner { kind: BannerKind::Warning, message: note }
                    } else {
                        // The clipboard gets the value, never the rendering: a
                        // blob copies as its hex, exactly as Ctrl+C over the cell.
                        // Rendered on click — see [`CopyRawButton`].
                        CopyRawButton { value: fetch.value.clone() }
                    }
                    div { class: "mt-2",
                        CellViewer {
                            value: fetch.value,
                            type_name: type_name.clone(),
                            truncated: capped.then_some(fetch.full_len),
                        }
                    }
                }
            },
        }
    }
}

/// A right-aligned "Copy raw" action for the expand popup: always
/// copies the raw cell value — the pane may be showing an image, a hex dump or
/// a JSON tree, which are for reading, not round-tripping.
///
/// Takes the [`Value`] and renders it **on click** rather than being handed a
/// finished string. [`raw_cell_text`] hexes a blob to twice its size, so the
/// eager form retained ~17 MB for an 8 MiB cell nobody had asked to copy —
/// beside a dump that deliberately stops at 16 KiB. Now that cost is paid once,
/// by the click that wants it, and dropped after.
#[component]
pub(super) fn CopyRawButton(value: Value) -> Element {
    rsx! {
        div { class: "mb-1 flex justify-end",
            button {
                class: BTN_SECONDARY_SM,
                title: "Copy the raw value (not the formatted view)",
                onclick: move |_| write_clipboard(&raw_cell_text(&value)),
                "Copy raw"
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::fixtures::*;
    use super::*;
    use crate::db::PREVIEW_BYTES;

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
        // the declared type, like the detail panel.
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
}
