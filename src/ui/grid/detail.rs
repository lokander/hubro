//! The row detail panel (FRE-109): one row of the grid as a vertical
//! column/value form, docked beside it.
//!
//! Its model is derived from the grid's focused cell rather than stored, so
//! the panel can never drift from the row the grid is showing. The editors it
//! opens are the same [`CellEditor`] the grid's cells use, wired through the
//! same [`edit_callbacks`].

use super::*;

/// Default width of the row detail panel in CSS pixels, and the range a drag
/// may take it to (FRE-109). The floor keeps a name/type header legible; the
/// ceiling keeps the grid — which the panel accompanies rather than replaces
/// — from being squeezed away.
pub(super) const DETAIL_WIDTH: f64 = 360.0;
pub(super) const DETAIL_MIN_WIDTH: f64 = 240.0;
pub(super) const DETAIL_MAX_WIDTH: f64 = 720.0;

/// Clamps a dragged panel width into the allowed range. A non-finite width
/// (a nonsense report from the drag listener) falls back to the default
/// rather than propagating a NaN into the style attribute.
pub(super) fn clamp_detail_width(width: f64) -> f64 {
    if !width.is_finite() {
        return DETAIL_WIDTH;
    }
    width.clamp(DETAIL_MIN_WIDTH, DETAIL_MAX_WIDTH)
}

/// One field of the row detail panel (FRE-109): a column of the focused row,
/// with everything the panel needs to show it, edit it, and follow it.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct DetailField {
    pub(super) column: String,
    /// Declared type, shown beside the name — via the Schema pane's
    /// [`display_type`], so the two views never disagree about what a column
    /// is (a Postgres enum reads as its type, not `USER-DEFINED`).
    pub(super) type_name: String,
    /// The cell as the grid holds it: for a previewed cell only the bounded
    /// prefix, which the panel replaces with the full value (FRE-33).
    pub(super) value: Value,
    pub(super) preview: Option<PreviewInfo>,
    pub(super) dirty: bool,
    pub(super) kind: EditorKind,
    pub(super) nullable: bool,
    /// The grid's own answer (`GridNavCell::editable`), which already folds in
    /// the resolved capability and the user's marking (FRE-87/FRE-111) — not
    /// a second resolution that could disagree with the cell beside it.
    pub(super) editable: bool,
    /// The foreign key this column belongs to, when following it leads
    /// somewhere: a NULL key references nothing (FRE-29).
    pub(super) fk: Option<ForeignKeyMeta>,
}

impl DetailField {
    /// Whether this field's full value is past [`FETCH_CELL_MAX_BYTES`], so it
    /// will render a note rather than an editor.
    ///
    /// Read off the preview the page already carries rather than recomputed,
    /// and known before the fetch resolves — the same answer
    /// [`CellFetch::capped`](crate::db::CellFetch) gives afterwards.
    pub(super) fn over_fetch_cap(&self) -> bool {
        self.preview
            .as_ref()
            .is_some_and(|preview| preview.full_len > FETCH_CELL_MAX_BYTES as u64)
    }
}

/// Where the focused row sits on the page — the panel's header line and its
/// Prev/Next bounds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct DetailPosition {
    /// 1-based position of the row within the fetched page.
    pub(super) number: usize,
    pub(super) total: usize,
}

impl DetailPosition {
    pub(super) fn has_prev(&self) -> bool {
        self.number > 1
    }

    /// Prev/Next stay inside the page, like the arrow keys they delegate to:
    /// paging is PageUp/PageDown's job, and a silent page flip under an open
    /// form would be a surprise.
    pub(super) fn has_next(&self) -> bool {
        self.number < self.total
    }
}

/// The focused row, reduced to what the row detail panel renders.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct RowDetail {
    /// Identifies the row across renders so the panel's fields — each owning
    /// a full-value fetch — remount when the focus moves to another row.
    pub(super) row_key: String,
    /// `None` for a row that can't be addressed (a view, a keyless table):
    /// nothing can be staged for it and its previews can't be loaded.
    pub(super) locator: Option<RowLocator>,
    pub(super) fields: Vec<DetailField>,
    /// The whole row by column — the source of an FK jump's equality filter.
    /// [`Shared`] because every field of the panel carries it (FRE-130).
    pub(super) row_values: Shared<HashMap<String, Value>>,
    pub(super) position: DetailPosition,
}

/// Reduces the focused row of `nav` to the panel's model (FRE-109).
///
/// The row is taken from the grid's focus (the selection model's focus
/// corner), not from any row the panel remembers for itself — and the panel's
/// Prev/Next move that same focus. One row, one place it lives.
///
/// `None` when there is nothing to show (an empty page).
pub(super) fn row_detail(
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
pub(super) enum RowStep {
    Prev,
    Next,
}

impl RowStep {
    /// The grid move this step delegates to, so a panel step and an arrow key
    /// resolve through exactly the same bounds logic.
    pub(super) fn grid_move(self) -> GridMove {
        match self {
            RowStep::Prev => GridMove::Up,
            RowStep::Next => GridMove::Down,
        }
    }
}

/// The row detail panel's drag-to-resize listener (FRE-109). Same shape as
/// [`GRID_SCROLL_JS`]: the drag is handled entirely in JS, which moves the
/// node itself and reports the resting width back once, on release — so a
/// drag costs no re-renders and the width still ends up in Rust, where it
/// survives a table switch. The move/up listeners are added on pointerdown
/// and removed on pointerup, so closing the panel leaves nothing behind.
pub(super) fn detail_resize_js() -> String {
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
pub(super) fn RowDetailPanel(
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
pub(super) fn RowDetailFields(
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
pub(super) fn RowDetailRow(
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
pub(super) fn RowDetailFullValue(
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
pub(super) fn RowDetailValue(
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
        let draft = open.and_then(|open| open.draft);
        let (on_commit, on_draft) = edit_callbacks(
            editing,
            row_key,
            field.column.clone(),
            editable_columns,
            move |value| state.stage_cell_edit(id, &table, locator.clone(), &column, value),
        );
        return rsx! {
            CellEditor {
                // A block wrapper, not a table cell: this is a form, not a row.
                block: true,
                kind: field.kind.clone(),
                dialect,
                nullable: field.nullable,
                initial: value,
                draft,
                on_commit,
                on_cancel: move |_| editing.set(None),
                // Input that doesn't parse is stashed rather than dropped
                // (FRE-74). The grid needs this because scrolling unmounts a
                // row mid-typing; the panel needs it because `RowDetailFields`
                // is keyed by row, so *every* row move — Prev/Next, an arrow
                // key, a click, the post-save refetch — remounts every field.
                // Without it the text vanished silently, which is worse than
                // the grid, whose editor stays open showing the parse error.
                on_draft,
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
    use crate::db::PREVIEW_BYTES;
    use crate::ui::grid::fixtures::*;

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
