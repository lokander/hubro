//! What the grid renders per row and per cell: the view model built from a
//! fetched page plus its stage, and the components that draw it — the header,
//! data rows, pending-insert rows, and the in-place editors.
//!
//! The view model is separate from the components on purpose: turning a
//! fetched page into rows is where the staged edits, row locators and
//! editability rules are resolved, and all of that is pure enough to unit
//! test without rendering anything.

use super::*;

/// One cell prepared for rendering.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct CellView {
    pub(super) column: String,
    /// The fetched value. For a truncated large cell (`preview` is `Some`)
    /// this is only a bounded PREVIEW — never stage it as an edit; the full
    /// value is loaded on demand (FRE-33).
    pub(super) value: Value,
    pub(super) dirty: bool,
    /// Full-value metadata when this cell is a truncated preview; `None` for a
    /// complete value (and always `None` for a dirty/staged cell, whose value
    /// is the user's full staged input).
    pub(super) preview: Option<PreviewInfo>,
    /// Row-level editability: the row has a locator, is not pending
    /// deletion, and this cell's fetched value is not a blob. Column-type
    /// restrictions (blob-typed columns) apply on top at render time.
    pub(super) editable: bool,
}

/// One fetched row prepared for rendering, staged state applied.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct RowView {
    /// [`RowLocator::key`] of `locator`, when the row is addressable.
    pub(super) key: Option<String>,
    /// How staged edits address this row (`None` when the table has no
    /// identity or a key column is missing from the fetched page).
    pub(super) locator: Option<RowLocator>,
    pub(super) deleted: bool,
    pub(super) cells: Vec<CellView>,
}

/// The fetched page reduced to exactly what the grid renders (FRE-130).
///
/// Built once per page/stage change by a memo and read by both the render and
/// the keyboard-navigation model, so the page is never re-derived by a focus
/// move, a copy or a checkbox tick. Structurally `PartialEq` (rather than
/// pointer-compared) because that comparison is what gates those readers: a
/// rebuild from unchanged inputs must re-render nothing.
#[derive(Debug, Default, Clone, PartialEq)]
pub(super) struct PageView {
    /// Visible column names: the fetched result's, or the schema's when a
    /// zero-row page carries none. [`Shared`] because the header list is also
    /// handed to every pending-insert row.
    pub(super) headers: Shared<Vec<String>>,
    /// The page's rows with the stage applied (see [`view_rows`]).
    pub(super) rows: Vec<RowView>,
    /// The rows "select all on this page" ticks (see [`selectable_rows`]).
    /// [`Shared`] because the checkbox handler has to own a copy.
    pub(super) selectable: Shared<Vec<(String, RowLocator)>>,
}

/// This page's selectable rows, in page order: those that are addressable and
/// not already pending delete. Backs both the header checkbox's ticked state
/// and what clicking it selects.
pub(super) fn selectable_rows(rows: &[RowView]) -> Vec<(String, RowLocator)> {
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
/// of the page is never copied per render.
///
/// `start..end` must be a valid range into `rows`. It is: it comes from
/// [`compute_visible_range`], which clamps `end` to the row count it was given
/// and can never return `end < start`, and the caller clamps both against the
/// rendered page's length again. The assert names that dependency instead of
/// leaving it to be rediscovered — the slice runs in the render path, so a
/// later change to `compute_visible_range` that broke the invariant would take
/// the grid down with it.
pub(super) fn window_rows(rows: &[RowView], start: usize, end: usize) -> Vec<(usize, RowView)> {
    debug_assert!(
        start <= end && end <= rows.len(),
        "visible range {start}..{end} is not a window into {} rows",
        rows.len(),
    );
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
pub(super) struct TableRenderMeta {
    /// The table's column names in schema order: the filter dropdown's options
    /// and the header fallback for a page with no rows to name them.
    pub(super) schema_columns: Vec<String>,
    /// Full column metadata, for the detail panel's declared types.
    pub(super) columns: Vec<ColumnMeta>,
    /// Per-column editor kind + nullability (see [`column_kinds_of`]).
    pub(super) column_kinds: HashMap<String, (EditorKind, bool)>,
    /// Foreign keys of this table, indexed by `col_to_fk` (FRE-29).
    pub(super) foreign_keys: Vec<ForeignKeyMeta>,
    /// Referencing column → index into `foreign_keys`; a column in several FKs
    /// takes the first (documented v1 limit).
    pub(super) col_to_fk: HashMap<String, usize>,
    /// Required-column flagging for pending inserts: NOT NULL + no default +
    /// not auto-assigned (see [`required_insert_columns`] for the per-backend
    /// rules). Unfilled required cells red-flag and block Save.
    pub(super) required: HashSet<String>,
}

impl TableRenderMeta {
    /// Reduces one table's introspected metadata to what rendering needs.
    /// `dialect` only feeds the required-column rules, which are per-backend;
    /// without a live connection nothing is flagged (there is nothing to save
    /// through either). Empty when the schema isn't loaded yet.
    pub(super) fn build(meta: Option<&TableMeta>, dialect: Option<Dialect>) -> Self {
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
            column_kinds: column_kinds_of(Some(meta), dialect),
            foreign_keys: meta.foreign_keys.clone(),
            col_to_fk,
            required: dialect
                .map(|dialect| required_insert_columns(meta, dialect))
                .unwrap_or_default(),
        }
    }

    /// Editor kind + nullability for one column; see [`column_kind`].
    pub(super) fn kind_of(&self, column: &str) -> (EditorKind, bool) {
        column_kind(column, &self.column_kinds)
    }

    /// One column's declared type, as the Schema pane spells it — the string
    /// the rich viewers classify on (FRE-115). Empty for a column the loaded
    /// schema doesn't name (a schema still loading, a result whose columns
    /// aren't the table's), which
    /// [`classify_column`](crate::db::classify_column) already reads as
    /// "unknown, so treat it as text".
    pub(super) fn type_of(&self, column: &str) -> String {
        self.columns
            .iter()
            .find(|meta| meta.name == column)
            .map(display_type)
            .unwrap_or_default()
    }

    /// The foreign key this column belongs to, when following it leads
    /// somewhere: a NULL key references nothing (FRE-29).
    pub(super) fn fk_of(&self, column: &str, value: &Value) -> Option<&ForeignKeyMeta> {
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
pub(super) fn view_rows(
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
pub(super) fn cell_display(cell: &CellView) -> String {
    match (&cell.value, cell.preview) {
        (Value::Text(preview), Some(_)) => format!("{preview}…"),
        (_, Some(info)) if info.binary => format!("<blob {}>", human_bytes(info.full_len)),
        _ => cell.value.display(),
    }
}

/// Stable list key for one row: the row key when the row is addressable,
/// else its page position.
pub(super) fn row_render_key(row: &RowView, index: usize) -> String {
    match &row.key {
        Some(key) => format!("r{key}"),
        None => format!("i{index}"),
    }
}

/// The staged-delete locators of a selection, in row-key order — a
/// deterministic order means identical selections always stage identical
/// change lists (stable failure indexes, stable confirm snapshots).
pub(super) fn selection_locators(selected: &HashMap<String, RowLocator>) -> Vec<RowLocator> {
    let mut keys: Vec<&String> = selected.keys().collect();
    keys.sort();
    keys.into_iter().map(|key| selected[key].clone()).collect()
}

/// Per-column editor kind + nullability from introspected metadata.
/// Database-assigned `GENERATED ALWAYS` columns become a read-only kind
/// rather than inviting doomed input. Empty when the schema isn't ready.
pub(super) fn column_kinds_of(
    meta: Option<&TableMeta>,
    dialect: Option<Dialect>,
) -> HashMap<String, (EditorKind, bool)> {
    meta.map(|meta| {
        meta.columns
            .iter()
            .map(|c| {
                let kind = if c.generated == Generated::Always {
                    EditorKind::Generated
                } else {
                    editor_kind(&c.type_name, &c.type_detail, dialect)
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
pub(super) fn cell_editable(
    cell: &CellView,
    column_kinds: &HashMap<String, (EditorKind, bool)>,
) -> bool {
    editable_for_kind(cell, &cell_kind(cell, column_kinds).0)
}

/// [`cell_editable`] for a cell whose column kind has already been resolved —
/// the row renderer looks each column up once and asks this (FRE-130), rather
/// than repeating the lookup per question.
pub(super) fn editable_for_kind(cell: &CellView, kind: &EditorKind) -> bool {
    cell.editable && !kind.is_read_only()
}

/// Editor kind + nullability for one column; columns missing from the
/// introspected metadata edit as nullable text.
pub(super) fn column_kind(
    column: &str,
    column_kinds: &HashMap<String, (EditorKind, bool)>,
) -> (EditorKind, bool) {
    column_kinds
        .get(column)
        .cloned()
        .unwrap_or((EditorKind::Text, true))
}

/// [`column_kind`] for a prepared cell.
pub(super) fn cell_kind(
    cell: &CellView,
    column_kinds: &HashMap<String, (EditorKind, bool)>,
) -> (EditorKind, bool) {
    column_kind(&cell.column, column_kinds)
}

/// The editable column after (`+1`) or before (`-1`) `current` in a row's
/// Tab order; `None` at the row's edge (the editor then just closes).
pub(super) fn step_column(columns: &[String], current: &str, delta: i32) -> Option<String> {
    let position = columns.iter().position(|c| c == current)?;
    let next = position as i64 + i64::from(delta);
    if next < 0 {
        return None;
    }
    columns.get(next as usize).cloned()
}

/// The `(on_commit, on_draft)` pair [`edit_callbacks`] builds. Boxed so the
/// four call sites name one type instead of two `impl Trait`s; the editor is
/// one cell at a time, so the allocation is not on any hot path.
pub(super) type EditCallbacks = (
    Box<dyn FnMut((Option<Value>, EditNav))>,
    Box<dyn FnMut(String)>,
);

/// The `on_commit` / `on_draft` pair every open [`CellEditor`] needs.
///
/// Four sites open one — a grid cell, a pending-insert cell, a truncated
/// cell's editor and a detail-panel field — and all four want the same two
/// behaviours: commit through the caller's `stage` step then walk to the
/// column `nav` asks for, and stash unparseable text against the row/column
/// it belongs to. `stage` is the one thing they genuinely differ in (a cell
/// edit vs. a pending insert's value), so it stays a caller-supplied closure.
///
/// The draft guard is why this is shared rather than repeated: it is
/// load-bearing for the one-editor invariant, not just for focus. Input that
/// doesn't parse is stashed rather than dropped (FRE-74), and `use_drop`
/// fires *after* whatever closed this editor has already chosen the next one
/// — so an unguarded stash resurrects the invalid editor and hijacks the
/// switch. Double-clicking another panel field reopened the old one, and
/// double-clicking a grid cell was swallowed entirely (the resurrected editor
/// stole the shared element id back, blurring the grid's). Every route to the
/// grid blurs the input first, which commits and closes it *unless* the text
/// doesn't parse — precisely the case the reverse guard in [`DataGrid`]
/// handles, and precisely the case an unguarded stash would undo.
pub(super) fn edit_callbacks(
    mut editing: Signal<Option<ActiveEdit>>,
    row_key: String,
    column: String,
    editable_columns: Shared<Vec<String>>,
    mut stage: impl FnMut(Value) + 'static,
) -> EditCallbacks {
    let commit_row_key = row_key.clone();
    let commit_column = column.clone();
    let on_commit = move |(value, nav): (Option<Value>, EditNav)| {
        if let Some(value) = value {
            stage(value);
        }
        // Tab walks the editable columns — in the detail panel that is the
        // field list, which is the same thing seen sideways.
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
    };
    let on_draft = move |text: String| {
        // Stash only while this cell is still the active edit — a deliberate
        // switch to another cell, or a sort/filter reset, must not be hijacked
        // back to the invalid editor.
        let still_active = editing
            .peek()
            .as_ref()
            .is_some_and(|active| active.is_on(&row_key, &column));
        if still_active {
            editing.set(Some(ActiveEdit {
                row_key: row_key.clone(),
                column: column.clone(),
                draft: Some(text),
            }));
        }
    };
    (Box::new(on_commit), Box::new(on_draft))
}

#[component]
pub(super) fn GridHeader(
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
pub(super) fn GridRow(
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
pub(super) struct RowCell {
    pub(super) cell: CellView,
    pub(super) kind: EditorKind,
    pub(super) nullable: bool,
    /// Row-level editability narrowed by the column's type — the same answer
    /// [`cell_editable`] gives.
    pub(super) editable: bool,
    /// The foreign key this cell can be followed through, if any.
    pub(super) fk: Option<ForeignKeyMeta>,
}

/// One cell of an editable-capable row: the display cell normally, or the
/// [`CellEditor`] while this cell is the grid's active edit. Commits stage
/// through [`AppState::stage_cell_edit`] — never the database — and Tab
/// commits walk `editable_columns`.
#[component]
pub(super) fn GridCellSlot(
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
        let draft = editing
            .read()
            .as_ref()
            .and_then(|active| active.draft.clone());
        let (on_commit, on_draft) =
            edit_callbacks(editing, row_key, column.clone(), editable_columns, {
                let table = table.clone();
                move |value| state.stage_cell_edit(id, &table, locator.clone(), &column, value)
            });
        rsx! {
            CellEditor {
                kind,
                dialect,
                nullable,
                initial: cell.value.clone(),
                draft,
                on_commit,
                on_cancel: move |_| editing.set(None),
                on_draft,
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
pub(super) fn InsertRow(
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
pub(super) fn InsertCellSlot(
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
        let default_table = table.clone();
        let default_column = column.clone();
        let draft = editing
            .read()
            .as_ref()
            .and_then(|active| active.draft.clone());
        let (on_commit, on_draft) =
            edit_callbacks(editing, row_key, column.clone(), editable_columns, {
                let table = table.clone();
                let column = column.clone();
                move |value| state.stage_insert_value(id, &table, insert_id, &column, value)
            });
        rsx! {
            CellEditor {
                kind,
                dialect,
                nullable,
                initial: override_value.clone().unwrap_or(Value::Null),
                draft,
                on_draft,
                on_commit,
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
pub(super) fn TruncatedCellEditor(
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
            let draft = editing
                .read()
                .as_ref()
                .and_then(|active| active.draft.clone());
            let (on_commit, on_draft) = edit_callbacks(
                editing,
                row_key.clone(),
                column.clone(),
                editable_columns.clone(),
                {
                    let table = table.clone();
                    let column = column.clone();
                    let locator = locator.clone();
                    move |value| state.stage_cell_edit(id, &table, locator.clone(), &column, value)
                },
            );
            rsx! {
                CellEditor {
                    kind,
                    dialect,
                    nullable,
                    initial,
                    draft,
                    on_draft,
                    on_commit,
                    on_cancel: move |_| editing.set(None),
                }
            }
        }
    }
}

/// The full-value body of the expand popup for a truncated cell (FRE-33):
/// loads the value via [`AppState::load_cell`] and hands it to [`CellViewer`]
/// (FRE-115) — a JSON tree, a picture, a hex dump or a text pane. A value over
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
            let capped = fetch.capped;
            // A capped fetch holds a prefix, so Copy raw is withdrawn rather
            // than handed a truncated value — the same refusal `plan_copy`
            // makes for this cell, worded identically (FRE-110). What is on
            // screen is the viewer's to describe; this says only what was
            // loaded and what that costs.
            let capped_note = capped.then(|| {
                format!(
                    "Value is very large; only the first {} was loaded. {}",
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
                    // The clipboard gets the value, never the rendering: a
                    // blob copies as its hex, exactly as Ctrl+C over the cell.
                    // Rendered on click — see [`CopyRawButton`].
                    CopyRawButton { value: fetch.value.clone() }
                }
                div { class: "mt-2",
                    CellViewer {
                        value: fetch.value.clone(),
                        type_name: type_name.clone(),
                        truncated: capped.then_some(fetch.full_len),
                    }
                }
            }
        }
    }
}

/// A right-aligned "Copy raw" action for the expand popup (FRE-77): always
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
                class: "rounded border border-slate-300 dark:border-slate-700 px-1.5 py-0.5 text-xs text-slate-500 dark:text-slate-400 hover:bg-slate-200 dark:hover:bg-slate-800 hover:text-slate-900 dark:hover:text-slate-100",
                title: "Copy the raw value (not the formatted view)",
                onclick: move |_| write_clipboard(&raw_cell_text(&value)),
                "Copy raw"
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::grid::fixtures::*;

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
                        kind: crate::ui::editing::NumericKind::Integer,
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
}
