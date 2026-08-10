//! The import dialog (FRE-112): pick how to read the file, see what that
//! produces, map its fields onto the table's columns, and start the import.
//!
//! Everything here is a *choice with a visible consequence*. Detection
//! ([`sniff_file`]) seeds the controls, and every one of them can be
//! overridden — the preview re-reads the head of the file on each change, so
//! the wrong delimiter or encoding is something you see rather than something
//! you find out about afterwards.
//!
//! Two of the controls decide what the import means rather than how the file
//! is read, and both are stated rather than assumed:
//!
//! - **empty fields** become NULL or an empty string ([`EmptyField`]) — CSV
//!   cannot tell the two apart, and silently picking one is how an import
//!   quietly rewrites every blank cell;
//! - **a row that can't be imported** aborts everything or is skipped and
//!   reported ([`ErrorMode`]). Abort is the default and the safe one; skip is
//!   a decision made here, before starting, because a half-imported table
//!   nobody asked for is worse than a failed import.
//!
//! The dialog never writes anything itself: pressing Import hands an
//! [`ImportRequest`] to [`AppState::start_import`], which runs it in a
//! background task and reports through the grid toolbar's status line.

use std::collections::HashMap;
use std::path::PathBuf;

use dioxus::prelude::*;

use crate::db::{
    is_importable, mapping_from_header, open_source, preview, sniff_file, ColumnBinding,
    ConnectionId, CsvDialect, EmptyField, Encoding, ErrorMode, ImportOptions, JsonShape,
    SourceField, SourceFormat, SourcePreview, TableMeta,
};

use super::notice::{Banner, BannerKind};
use super::state::{AppState, ImportRequest};

/// How many records the preview reads. Enough to show the shape of the file
/// without reading a large one twice.
const PREVIEW_ROWS: usize = 8;

/// The delimiters the dialog offers, with their labels. A file using
/// something else is out of scope: these are what the formats people export
/// actually use, and the detection covers the same four.
const DELIMITERS: [(u8, &str); 4] = [
    (b',', "Comma ,"),
    (b';', "Semicolon ;"),
    (b'\t', "Tab"),
    (b'|', "Pipe |"),
];

const QUOTES: [(u8, &str); 2] = [(b'"', "Double \""), (b'\'', "Single '")];

/// The value a `<select>` option carries for "don't import this field".
const UNMAPPED: &str = "";

#[component]
pub fn ImportDialog(
    id: ConnectionId,
    /// The target table — its columns are what the fields map onto, and what
    /// supplies the types every value is coerced to.
    table: TableMeta,
    path: PathBuf,
    /// Why importing into this object is refused, when it is (FRE-87/111).
    /// Present means the dialog explains and offers nothing to press: the
    /// same sentence the disabled Save button shows.
    refusal: Option<String>,
    on_close: EventHandler<()>,
) -> Element {
    let state = use_context::<AppState>();
    // Detection runs once, on open: it is evidence for the initial control
    // values, not a live answer that should fight the user's overrides.
    let detected = use_hook({
        let path = path.clone();
        move || sniff_file(&path).ok()
    });
    let mut format = use_signal(|| match detected {
        Some(sniff) => sniff.format,
        None => SourceFormat::Csv(CsvDialect::default()),
    });
    let mut encoding = use_signal(|| detected.map(|s| s.encoding).unwrap_or_default());
    let mut empty_field = use_signal(EmptyField::default);
    let mut on_error = use_signal(ErrorMode::default);
    // Per-field mapping overrides, keyed by the field's label; a field with
    // no entry keeps whatever the name matching chose. Holding only the
    // *overrides* means changing the delimiter re-derives the defaults
    // without discarding a choice the user made about a field that survived.
    let mut overrides = use_signal(HashMap::<String, String>::new);

    // Re-read the head of the file whenever how-to-read-it changes. A memo,
    // not an effect: it is derived state, and the PartialEq gate keeps an
    // unrelated re-render from re-reading the file.
    let preview_path = path.clone();
    let file_preview: Memo<Result<SourcePreview, String>> = use_memo(move || {
        let mut source = open_source(&preview_path, format(), encoding())
            .map_err(|e| format!("opening the file failed: {e}"))?;
        preview(source.as_mut(), PREVIEW_ROWS).map_err(|e| match e.line {
            Some(line) => format!("line {line}: {}", e.message),
            None => e.message,
        })
    });

    let columns_table = table.clone();
    let mapping: Memo<Vec<ColumnBinding>> = use_memo(move || match file_preview() {
        Ok(preview) => effective_mapping(&columns_table, &preview, &overrides.read()),
        Err(_) => Vec::new(),
    });
    let problem: Memo<Option<String>> = use_memo(move || mapping_problem(&mapping()));

    let file_name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| path.display().to_string());
    let is_csv = matches!(format(), SourceFormat::Csv(_));
    let start = {
        let table = table.clone();
        let path = path.clone();
        move |_| {
            let request = ImportRequest {
                path: path.clone(),
                format: format.peek().to_owned(),
                encoding: *encoding.peek(),
                table: table.clone(),
                options: ImportOptions {
                    mapping: mapping.peek().clone(),
                    empty_field: *empty_field.peek(),
                    on_error: *on_error.peek(),
                },
            };
            state.start_import(id, request);
            on_close.call(());
        }
    };

    rsx! {
        div {
            class: "fixed inset-0 z-50 flex items-start justify-center overflow-y-auto bg-black/40 p-4 outline-none",
            // Focused on mount so Escape works before anything is clicked,
            // matching the connect form (the window key listener ignores
            // keys while a field is focused).
            tabindex: "-1",
            onmounted: super::js::focus_on_mount,
            onkeydown: move |evt: KeyboardEvent| {
                if evt.code() == Code::Escape {
                    evt.stop_propagation();
                    on_close.call(());
                }
            },
            div {
                class: "w-full max-w-3xl rounded-lg bg-white dark:bg-slate-900 p-4 shadow-xl",
                onclick: move |evt| evt.stop_propagation(),
                div { class: "mb-3 flex items-baseline justify-between gap-2",
                    h2 { class: "text-sm font-semibold text-slate-900 dark:text-slate-100",
                        "Import into {table.name}"
                    }
                    span { class: "truncate text-xs text-slate-500 dark:text-slate-400", title: "{path.display()}",
                        "{file_name}"
                    }
                }

                if let Some(reason) = refusal.clone() {
                    Banner { kind: BannerKind::Error, message: reason }
                } else {
                    // ---- How to read the file -------------------------------
                    div { class: "mb-3 flex flex-wrap items-center gap-3 text-xs text-slate-600 dark:text-slate-300",
                        label { class: "flex items-center gap-1",
                            "Format"
                            select {
                                class: SELECT_CLASS,
                                value: "{format_key(&format())}",
                                onchange: move |evt| {
                                    let current = *format.peek();
                                    format.set(format_from_key(&evt.value(), current));
                                },
                                for (key, label) in [
                                    ("csv", "CSV"),
                                    ("json-array", "JSON array"),
                                    ("json-lines", "JSON per line"),
                                ] {
                                    option {
                                        key: "{key}",
                                        value: "{key}",
                                        selected: format_key(&format()) == key,
                                        "{label}"
                                    }
                                }
                            }
                        }
                        if is_csv {
                            label { class: "flex items-center gap-1",
                                "Delimiter"
                                select {
                                    class: SELECT_CLASS,
                                    value: "{csv_of(&format()).delimiter}",
                                    onchange: move |evt| {
                                        let mut dialect = csv_of(&format.peek());
                                        if let Ok(byte) = evt.value().parse::<u8>() {
                                            dialect.delimiter = byte;
                                        }
                                        format.set(SourceFormat::Csv(dialect));
                                    },
                                    for (byte, label) in DELIMITERS {
                                        option {
                                            key: "{byte}",
                                            value: "{byte}",
                                            selected: csv_of(&format()).delimiter == byte,
                                            "{label}"
                                        }
                                    }
                                }
                            }
                            label { class: "flex items-center gap-1",
                                "Quote"
                                select {
                                    class: SELECT_CLASS,
                                    value: "{csv_of(&format()).quote}",
                                    onchange: move |evt| {
                                        let mut dialect = csv_of(&format.peek());
                                        if let Ok(byte) = evt.value().parse::<u8>() {
                                            dialect.quote = byte;
                                        }
                                        format.set(SourceFormat::Csv(dialect));
                                    },
                                    for (byte, label) in QUOTES {
                                        option {
                                            key: "{byte}",
                                            value: "{byte}",
                                            selected: csv_of(&format()).quote == byte,
                                            "{label}"
                                        }
                                    }
                                }
                            }
                            label { class: "flex items-center gap-1",
                                input {
                                    r#type: "checkbox",
                                    checked: csv_of(&format()).has_header,
                                    onchange: move |evt| {
                                        let mut dialect = csv_of(&format.peek());
                                        dialect.has_header = evt.checked();
                                        format.set(SourceFormat::Csv(dialect));
                                    },
                                }
                                "First row is a header"
                            }
                        }
                        label { class: "flex items-center gap-1",
                            "Encoding"
                            select {
                                class: SELECT_CLASS,
                                value: if encoding() == Encoding::Latin1 { "latin1" } else { "utf8" },
                                onchange: move |evt| {
                                    encoding.set(match evt.value().as_str() {
                                        "latin1" => Encoding::Latin1,
                                        _ => Encoding::Utf8,
                                    });
                                },
                                option {
                                    value: "utf8",
                                    selected: encoding() == Encoding::Utf8,
                                    "{Encoding::Utf8.label()}"
                                }
                                option {
                                    value: "latin1",
                                    selected: encoding() == Encoding::Latin1,
                                    "{Encoding::Latin1.label()}"
                                }
                            }
                        }
                    }

                    // ---- What the values mean -------------------------------
                    div { class: "mb-3 flex flex-wrap items-center gap-3 text-xs text-slate-600 dark:text-slate-300",
                        label { class: "flex items-center gap-1",
                            "Empty fields"
                            select {
                                class: SELECT_CLASS,
                                value: if empty_field() == EmptyField::EmptyText { "text" } else { "null" },
                                onchange: move |evt| {
                                    empty_field.set(match evt.value().as_str() {
                                        "text" => EmptyField::EmptyText,
                                        _ => EmptyField::Null,
                                    });
                                },
                                option {
                                    value: "null",
                                    selected: empty_field() == EmptyField::Null,
                                    "become NULL"
                                }
                                option {
                                    value: "text",
                                    selected: empty_field() == EmptyField::EmptyText,
                                    "become an empty string"
                                }
                            }
                        }
                        label { class: "flex items-center gap-1",
                            "A row that can't be imported"
                            select {
                                class: SELECT_CLASS,
                                value: if on_error() == ErrorMode::Skip { "skip" } else { "abort" },
                                onchange: move |evt| {
                                    on_error.set(match evt.value().as_str() {
                                        "skip" => ErrorMode::Skip,
                                        _ => ErrorMode::Abort,
                                    });
                                },
                                option {
                                    value: "abort",
                                    selected: on_error() == ErrorMode::Abort,
                                    "aborts the whole import"
                                }
                                option {
                                    value: "skip",
                                    selected: on_error() == ErrorMode::Skip,
                                    "is skipped and reported"
                                }
                            }
                        }
                    }
                    p { class: "mb-3 text-xs text-slate-500 dark:text-slate-400",
                        if on_error() == ErrorMode::Skip {
                            "Rows that can't be imported will be skipped and listed by line. Everything else is committed together."
                        } else {
                            "The import runs in one transaction: if any row can't be imported, nothing is written at all."
                        }
                    }

                    // ---- Preview and mapping --------------------------------
                    match file_preview() {
                        Err(err) => rsx! { Banner { kind: BannerKind::Error, message: err } },
                        Ok(preview) if preview.fields.is_empty() => rsx! {
                            Banner {
                                kind: BannerKind::Warning,
                                message: "This file has no fields to import.".to_string(),
                            }
                        },
                        Ok(preview) => rsx! {
                            div { class: "mb-3 max-h-64 overflow-auto rounded border border-slate-200 dark:border-slate-800",
                                table { class: "w-full text-left text-xs",
                                    thead { class: "sticky top-0 bg-slate-100 dark:bg-slate-800",
                                        tr {
                                            for (index, label) in preview.labels.iter().enumerate() {
                                                th { key: "{index}", class: "px-2 py-1 font-medium text-slate-700 dark:text-slate-200",
                                                    div { class: "truncate", title: "{label}", "{label}" }
                                                    ColumnPicker {
                                                        table: table.clone(),
                                                        label: label.clone(),
                                                        selected: selected_column(&mapping(), &preview.fields[index]),
                                                        on_pick: {
                                                            let label = label.clone();
                                                            move |column: String| {
                                                                overrides.write().insert(label.clone(), column);
                                                            }
                                                        },
                                                    }
                                                }
                                            }
                                        }
                                    }
                                    tbody {
                                        for (row_index, row) in preview.rows.iter().enumerate() {
                                            tr { key: "{row_index}", class: "border-t border-slate-200 dark:border-slate-800",
                                                for (cell_index, cell) in row.iter().enumerate() {
                                                    td { key: "{cell_index}", class: "max-w-40 truncate px-2 py-1 text-slate-600 dark:text-slate-300",
                                                        title: "{cell}",
                                                        "{cell}"
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        },
                    }

                    if let Some(problem) = problem() {
                        Banner { kind: BannerKind::Warning, message: problem }
                    }
                }

                div { class: "flex items-center justify-end gap-2",
                    button {
                        class: "rounded px-3 py-1.5 text-xs text-slate-600 dark:text-slate-300 hover:bg-slate-200 dark:hover:bg-slate-800",
                        onclick: move |_| on_close.call(()),
                        "Cancel"
                    }
                    button {
                        class: "rounded bg-sky-600 px-3 py-1.5 text-xs font-medium text-white hover:bg-sky-500 disabled:opacity-50",
                        disabled: refusal.is_some() || problem().is_some(),
                        onclick: start,
                        "Import"
                    }
                }
            }
        }
    }
}

/// A `<select>` shows whichever `<option>` carries `selected` — setting a
/// `value` attribute on the select itself does nothing, which is how every
/// picker in this dialog first came up reading "don't import" while the
/// mapping underneath was correct.
const SELECT_CLASS: &str = "rounded border border-slate-300 dark:border-slate-700 bg-white dark:bg-slate-800 px-1 py-0.5 text-xs text-slate-900 dark:text-slate-100";

/// One field's target-column picker.
#[component]
fn ColumnPicker(
    table: TableMeta,
    label: String,
    selected: Option<String>,
    on_pick: EventHandler<String>,
) -> Element {
    let current = selected.clone().unwrap_or_else(|| UNMAPPED.to_string());
    rsx! {
        select {
            class: "mt-0.5 w-full {SELECT_CLASS}",
            title: "Which column of the table {label} is imported into",
            value: "{current}",
            onchange: move |evt| on_pick.call(evt.value()),
            option {
                value: "{UNMAPPED}",
                selected: current == UNMAPPED,
                "— don't import —"
            }
            for column in table.columns.iter().filter(|c| is_importable(c)) {
                option {
                    key: "{column.name}",
                    value: "{column.name}",
                    selected: current == column.name,
                    "{column.name} ({column.type_name})"
                }
            }
        }
    }
}

fn label_at(labels: &[String], index: usize) -> String {
    labels.get(index).cloned().unwrap_or_default()
}

/// The column a field is currently bound to, if any.
fn selected_column(mapping: &[ColumnBinding], field: &SourceField) -> Option<String> {
    mapping
        .iter()
        .find(|binding| &binding.source == field)
        .map(|binding| binding.column.clone())
}

/// The mapping the import will run with: name matching for the defaults, with
/// the user's per-field overrides applied on top.
///
/// A field whose override is the empty string is deliberately left out — that
/// is what "don't import" means, and the column then takes the database's own
/// default rather than a NULL nobody asked for.
fn effective_mapping(
    table: &TableMeta,
    preview: &SourcePreview,
    overrides: &HashMap<String, String>,
) -> Vec<ColumnBinding> {
    let defaults: Vec<ColumnBinding> = match &preview.header {
        Some(names) => mapping_from_header(table, names),
        None => crate::db::default_mapping(table, &preview.fields),
    };
    let mut bindings = Vec::new();
    for (index, field) in preview.fields.iter().enumerate() {
        let label = label_at(&preview.labels, index);
        let column = match overrides.get(&label) {
            Some(chosen) if chosen == UNMAPPED => continue,
            Some(chosen) => chosen.clone(),
            None => match defaults.iter().find(|b| &b.source == field) {
                Some(binding) => binding.column.clone(),
                None => continue,
            },
        };
        bindings.push(ColumnBinding {
            source: field.clone(),
            column,
        });
    }
    bindings
}

/// Why this mapping can't be imported yet, or `None` when it can. The same
/// two conditions `run_import` refuses on, checked here so the button is
/// disabled with a reason instead of failing after the click.
fn mapping_problem(mapping: &[ColumnBinding]) -> Option<String> {
    if mapping.is_empty() {
        return Some("Choose at least one column to import into.".to_string());
    }
    for (index, binding) in mapping.iter().enumerate() {
        if mapping[..index].iter().any(|b| b.column == binding.column) {
            return Some(format!(
                "Two fields are mapped to \"{}\" — each column can only be imported once.",
                binding.column
            ));
        }
    }
    None
}

/// The `<select>` value for a format, and back. Kept as one pair so the two
/// directions cannot disagree; switching to CSV keeps whatever dialect was
/// detected rather than resetting it.
fn format_key(format: &SourceFormat) -> &'static str {
    match format {
        SourceFormat::Csv(_) => "csv",
        SourceFormat::Json(JsonShape::Array) => "json-array",
        SourceFormat::Json(JsonShape::Lines) => "json-lines",
    }
}

fn format_from_key(key: &str, current: SourceFormat) -> SourceFormat {
    match key {
        "json-array" => SourceFormat::Json(JsonShape::Array),
        "json-lines" => SourceFormat::Json(JsonShape::Lines),
        _ => SourceFormat::Csv(csv_of(&current)),
    }
}

/// The CSV dialect a format carries, or the default when it is not CSV — so
/// the delimiter/quote/header controls have something to show while the user
/// switches back and forth.
fn csv_of(format: &SourceFormat) -> CsvDialect {
    match format {
        SourceFormat::Csv(dialect) => *dialect,
        SourceFormat::Json(_) => CsvDialect::default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{Generated, TableKind, TypeDetail};

    fn column(name: &str, generated: Generated) -> crate::db::ColumnMeta {
        crate::db::ColumnMeta {
            name: name.into(),
            type_name: "text".into(),
            nullable: true,
            primary_key_position: None,
            default: None,
            generated,
            type_detail: TypeDetail::Plain,
        }
    }

    fn table() -> TableMeta {
        TableMeta {
            schema: None,
            name: "people".into(),
            kind: TableKind::Table,
            columns: vec![
                column("id", Generated::Always),
                column("name", Generated::Never),
                column("age", Generated::Never),
            ],
            indexes: vec![],
            foreign_keys: vec![],
            restriction: None,
            internal: None,
            kind_label: None,
        }
    }

    fn preview_of(labels: &[&str], header: bool) -> SourcePreview {
        let labels: Vec<String> = labels.iter().map(|l| (*l).to_string()).collect();
        SourcePreview {
            fields: (0..labels.len()).map(SourceField::Index).collect(),
            header: header.then(|| labels.clone()),
            labels,
            rows: vec![],
        }
    }

    #[test]
    fn the_default_mapping_matches_by_name_and_skips_database_assigned_columns() {
        let preview = preview_of(&["name", "age", "extra"], true);
        let mapping = effective_mapping(&table(), &preview, &HashMap::new());
        assert_eq!(
            mapping
                .iter()
                .map(|b| b.column.as_str())
                .collect::<Vec<_>>(),
            vec!["name", "age"],
            "a field matching nothing stays unmapped"
        );
        assert_eq!(mapping_problem(&mapping), None);
    }

    #[test]
    fn an_override_redirects_one_field_and_leaves_the_others_alone() {
        let preview = preview_of(&["name", "age"], true);
        let overrides = HashMap::from([("age".to_string(), "name".to_string())]);
        let mapping = effective_mapping(&table(), &preview, &overrides);
        // Both now point at "name", which is refused with a reason rather
        // than sent to the database to fail there.
        assert_eq!(mapping.len(), 2);
        let problem = mapping_problem(&mapping).unwrap();
        assert!(problem.contains("name"), "{problem}");
    }

    #[test]
    fn the_empty_override_means_do_not_import_this_field() {
        let preview = preview_of(&["name", "age"], true);
        let overrides = HashMap::from([("age".to_string(), UNMAPPED.to_string())]);
        let mapping = effective_mapping(&table(), &preview, &overrides);
        assert_eq!(
            mapping
                .iter()
                .map(|b| b.column.as_str())
                .collect::<Vec<_>>(),
            vec!["name"]
        );
    }

    #[test]
    fn nothing_mapped_is_reported_rather_than_started() {
        let problem = mapping_problem(&[]).unwrap();
        assert!(problem.contains("at least one"), "{problem}");
    }

    #[test]
    fn a_headerless_file_binds_positionally() {
        // No header: the labels are positional placeholders and the fields
        // bind to the importable columns in order — "id" is generated, so
        // position 0 is "name".
        let preview = preview_of(&["Column 1", "Column 2"], false);
        let mapping = effective_mapping(&table(), &preview, &HashMap::new());
        assert_eq!(
            mapping
                .iter()
                .map(|b| b.column.as_str())
                .collect::<Vec<_>>(),
            vec!["name", "age"]
        );
    }

    #[test]
    fn the_format_select_round_trips_and_keeps_the_detected_dialect() {
        let detected = SourceFormat::Csv(CsvDialect {
            delimiter: b';',
            quote: b'\'',
            has_header: false,
        });
        assert_eq!(format_key(&detected), "csv");
        // Away to JSON and back: the CSV settings are not silently reset.
        let json = format_from_key("json-lines", detected);
        assert_eq!(json, SourceFormat::Json(JsonShape::Lines));
        assert_eq!(format_from_key("csv", detected), detected);
        for key in ["csv", "json-array", "json-lines"] {
            assert_eq!(format_key(&format_from_key(key, detected)), key);
        }
    }
}
