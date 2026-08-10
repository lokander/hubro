//! Copying a cell selection to the clipboard (FRE-110): planning what the
//! selection covers, refusing what cannot be copied faithfully, fetching any
//! truncated cells first, and writing the result out.
//!
//! The refusals are the substance. A previewed cell holds only a prefix, so
//! copying it as if it were the value would hand the user silently truncated
//! data — worse than copying nothing.

use super::*;

/// The clipboard formats in copy-as menu order (FRE-110). TSV leads: it is
/// what the plain shortcut produces and what spreadsheets want.
pub(super) const COPY_FORMATS: [CopyFormat; 6] = [
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
pub(super) struct CopyStatus {
    pub(super) text: String,
    pub(super) error: bool,
}

impl CopyStatus {
    pub(super) fn ok(text: String) -> Self {
        CopyStatus { text, error: false }
    }

    pub(super) fn failed(text: String) -> Self {
        CopyStatus { text, error: true }
    }

    pub(super) fn class(&self) -> &'static str {
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
pub(super) struct CopyContext {
    pub(super) state: AppState,
    pub(super) id: ConnectionId,
    pub(super) table: TableRef,
    /// `None` when the connection is gone. Only [`CopyFormat::Insert`] needs
    /// it, and that format refuses rather than assuming a dialect — see
    /// [`CopyRefusal::UnknownDialect`].
    pub(super) dialect: Option<Dialect>,
    pub(super) status: Signal<Option<CopyStatus>>,
}

/// One cell of a planned copy: a value already held in full, or a ticket to
/// load one the grid holds only a bounded preview of (FRE-33). Copying a
/// preview would put silently truncated data on the clipboard, which is the
/// one thing FRE-110 must not do.
#[derive(Debug, Clone, PartialEq)]
pub(super) enum CopyCell {
    Ready(Value),
    Fetch { locator: RowLocator, column: String },
}

/// A copy reduced to what it needs: the selected column names and the
/// selected cells, row-major.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct CopyPlan {
    pub(super) columns: Vec<String>,
    pub(super) rows: Vec<Vec<CopyCell>>,
}

/// Why a copy was refused outright (FRE-110). Refusing beats truncating: a
/// truncated INSERT is still valid SQL and will run, writing wrong data with
/// no error anywhere.
#[derive(Debug, Clone, PartialEq)]
pub(super) enum CopyRefusal {
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
    pub(super) fn message(&self) -> String {
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
pub(super) fn plan_copy(nav: &GridNav, selection: Selection) -> Result<CopyPlan, CopyRefusal> {
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
pub(super) fn start_copy(
    ctx: &CopyContext,
    nav: &GridNav,
    selection: Selection,
    format: Option<CopyFormat>,
) {
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
pub(super) fn copy_summary(raw: bool, format: CopyFormat, rows: usize, cols: usize) -> String {
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
pub(super) fn write_clipboard(text: &str) {
    let json = js_string(text);
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::PREVIEW_BYTES;
    use crate::ui::grid::fixtures::*;

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
}
