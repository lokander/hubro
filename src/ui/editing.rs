//! Inline cell editing (FRE-24): type-aware editor kinds, input validation,
//! and the in-place `CellEditor` component the grid mounts over a cell.
//!
//! Editor kinds derive from the introspected column type
//! ([`ColumnMeta::type_name`](crate::db::ColumnMeta)) by **case-insensitive
//! substring matching**, so both SQLite declared types ("INTEGER",
//! "VARCHAR(40)", possibly empty) and Postgres `data_type` strings
//! ("integer", "timestamp without time zone", "jsonb") map without a
//! per-backend table:
//!
//! - `blob` / `bytea` → read-only (blobs are not editable yet)
//! - `bool` → checkbox
//! - `json` → multiline text, validated with serde_json before staging
//! - `date` / `time` / `timestamp` / `interval` → plain text staged as
//!   [`Value::Text`]; the backend validates on save. Deliberately no custom
//!   picker: ISO-ish text is unambiguous, works for every temporal flavor
//!   (including intervals), and the save-time cast reports bad input.
//!   (Checked before the numeric rule so "interval"'s `int` substring
//!   doesn't misfire.)
//! - `numeric` / `decimal` → validated number, staged as [`Value::Integer`]
//!   when it parses as i64 and as [`Value::Text`] otherwise — an exact
//!   decimal must not round-trip through f64
//! - `int` / `serial` / `real` / `float` / `double` → validated number,
//!   staged as [`Value::Integer`] or [`Value::Real`]
//! - everything else (including an empty/unknown declared type, and
//!   Postgres `point`, whose `int` substring is explicitly ignored) → text
//!
//! Values committed here are only ever **staged** (via
//! [`AppState::stage_cell_edit`](super::state::AppState::stage_cell_edit));
//! nothing touches the database until the user saves.

use dioxus::prelude::*;

use crate::db::{Dialect, Value};

/// Which editor a column gets, derived by [`editor_kind`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditorKind {
    /// Free-form text (also the fallback for unknown declared types).
    Text,
    /// Validated number. `decimal` marks exact-precision columns
    /// (numeric/decimal): non-integer input stays text so precision
    /// survives (the backend casts it).
    Numeric { decimal: bool },
    /// Checkbox; the staged value is dialect-specific (see [`bool_value`]).
    Bool,
    /// Multiline text validated as JSON before staging.
    Json,
    /// Date/time/timestamp/interval: ISO-ish text staged as text.
    DateTime,
    /// Blob/bytea columns are read-only for now.
    Blob,
}

/// Derives the editor kind from a declared column type (see the module docs
/// for the exact rules and their order).
pub fn editor_kind(type_name: &str) -> EditorKind {
    let t = type_name.to_ascii_lowercase();
    if t.is_empty() {
        return EditorKind::Text;
    }
    if t.contains("blob") || t.contains("bytea") {
        return EditorKind::Blob;
    }
    if t.contains("bool") {
        return EditorKind::Bool;
    }
    if t.contains("json") {
        return EditorKind::Json;
    }
    if t.contains("date") || t.contains("time") || t.contains("interval") {
        return EditorKind::DateTime;
    }
    if t.contains("point") {
        // Postgres "point" (geometric) contains "int" but is not a number.
        return EditorKind::Text;
    }
    if t.contains("numeric") || t.contains("decimal") {
        return EditorKind::Numeric { decimal: true };
    }
    if ["int", "serial", "real", "float", "double"]
        .iter()
        .any(|n| t.contains(n))
    {
        return EditorKind::Numeric { decimal: false };
    }
    EditorKind::Text
}

/// The staged value for a boolean checkbox, per backend:
///
/// - SQLite has no boolean storage class — `0`/`1` integers are what its
///   numeric affinity stores natively, so stage [`Value::Integer`].
/// - Postgres `boolean` columns reject a bound integer; stage the text
///   `"true"`/`"false"`, which the staged SQL's `::boolean` cast (and
///   Postgres' own literal parsing) accepts.
pub fn bool_value(dialect: Dialect, checked: bool) -> Value {
    match dialect {
        Dialect::Sqlite => Value::Integer(i64::from(checked)),
        Dialect::Postgres => Value::Text(if checked { "true" } else { "false" }.into()),
    }
}

/// Whether a fetched value reads as "checked" when a boolean cell opens.
/// Covers both backends' renderings: SQLite integers and Postgres
/// "true"/"false" text (plus common literal spellings).
pub fn bool_checked(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Integer(i) => *i != 0,
        Value::Real(r) => *r != 0.0,
        Value::Text(t) => matches!(
            t.to_ascii_lowercase().as_str(),
            "true" | "t" | "yes" | "on" | "1"
        ),
        Value::Blob(_) => false,
    }
}

/// Parses committed editor text into the [`Value`] to stage, per kind.
/// `Err` is the human-readable validation message shown inline; nothing is
/// staged then.
pub fn parse_input(kind: EditorKind, dialect: Dialect, text: &str) -> Result<Value, String> {
    match kind {
        EditorKind::Text | EditorKind::DateTime => Ok(Value::Text(text.to_string())),
        EditorKind::Json => match serde_json::from_str::<serde_json::Value>(text) {
            Ok(_) => Ok(Value::Text(text.to_string())),
            Err(err) => Err(format!("invalid JSON: {err}")),
        },
        EditorKind::Numeric { decimal } => parse_numeric(text, decimal),
        EditorKind::Bool => Ok(bool_value(
            dialect,
            bool_checked(&Value::Text(text.trim().to_string())),
        )),
        EditorKind::Blob => Err("blobs are read-only".to_string()),
    }
}

/// Numeric validation: integers stage as [`Value::Integer`]; other numbers
/// stage as [`Value::Real`] — except on exact-precision columns
/// (`decimal`), where the original text is staged so e.g. a 30-digit
/// numeric never rounds through f64. Non-numbers (and inf/NaN) are
/// rejected.
fn parse_numeric(text: &str, decimal: bool) -> Result<Value, String> {
    let t = text.trim();
    if t.is_empty() {
        return Err("enter a number (or use the ∅ NULL button)".to_string());
    }
    if let Ok(i) = t.parse::<i64>() {
        return Ok(Value::Integer(i));
    }
    match t.parse::<f64>() {
        Ok(f) if f.is_finite() => Ok(if decimal {
            Value::Text(t.to_string())
        } else {
            Value::Real(f)
        }),
        _ => Err(format!("not a number: {t}")),
    }
}

/// Where the editor hands off after a commit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditNav {
    /// Close the editor (Enter, blur, the NULL button).
    Stay,
    /// Move to the next editable cell in the row (Tab).
    Next,
    /// Move to the previous editable cell in the row (Shift+Tab).
    Prev,
}

/// The in-place cell editor. Rendered by the grid as the `td` of the active
/// cell; `initial` is the staged value when one exists, else the fetched
/// value.
///
/// Interaction model (FRE-24):
/// - Enter commits (stages) and closes; in the JSON editor Shift+Enter
///   inserts a newline instead.
/// - Escape cancels without staging.
/// - Tab / Shift+Tab commit and ask the grid to move along the row.
/// - Blur commits — clicking elsewhere behaves like Enter (documented
///   choice: losing typed input on a stray click would be worse than an
///   unexpected staged cell, which Discard undoes).
/// - An *unmodified* editor never stages on commit — so opening a cell and
///   clicking away (or Tab-walking across a row) leaves no dirty marks,
///   and a NULL shown as empty text does not silently become ''.
///   Consequence: to turn a NULL into the empty string, type something and
///   delete it (the ∅ button covers the reverse).
/// - The ∅ NULL button stages an explicit NULL — distinct from committing
///   an empty string. Hidden on NOT NULL columns.
/// - Validation failures (numeric, JSON) show inline and keep the editor
///   open; nothing is staged.
/// - Checkbox (boolean) editors stage-and-close immediately on toggle.
#[component]
pub fn CellEditor(
    kind: EditorKind,
    dialect: Dialect,
    nullable: bool,
    initial: Value,
    on_commit: EventHandler<(Option<Value>, EditNav)>,
    on_cancel: EventHandler<()>,
) -> Element {
    let initial_for_text = initial.clone();
    let mut text = use_signal(move || match &initial_for_text {
        Value::Null => String::new(),
        value => value.display(),
    });
    let initial_for_checked = initial.clone();
    let mut checked = use_signal(move || bool_checked(&initial_for_checked));
    let mut error = use_signal(|| Option::<String>::None);
    // Nothing typed/toggled yet: committing stages nothing (see above).
    let mut modified = use_signal(|| false);
    // Set once the editor committed or cancelled, so the blur that follows
    // closing (the input unmounts / loses focus) cannot double-commit.
    let mut finished = use_signal(|| false);

    let mut commit = move |nav: EditNav| {
        if finished() {
            return;
        }
        if !modified() {
            finished.set(true);
            match nav {
                EditNav::Stay => on_cancel.call(()),
                nav => on_commit.call((None, nav)),
            }
            return;
        }
        let parsed = if kind == EditorKind::Bool {
            Ok(bool_value(dialect, checked()))
        } else {
            parse_input(kind, dialect, &text())
        };
        match parsed {
            Ok(value) => {
                finished.set(true);
                on_commit.call((Some(value), nav));
            }
            Err(message) => error.set(Some(message)),
        }
    };

    let multiline = kind == EditorKind::Json;
    // Match on the physical key code, not `key()`: WebKitGTK reports
    // Shift+Tab with a non-"Tab" key name (ISO_Left_Tab), which would fall
    // through to native backward tab-navigation and close the editor via
    // blur instead of running the Shift+Tab commit.
    let on_key = move |evt: KeyboardEvent| match evt.code() {
        Code::Enter | Code::NumpadEnter => {
            if multiline && evt.modifiers().shift() {
                return; // newline inside the JSON editor
            }
            evt.prevent_default();
            commit(EditNav::Stay);
        }
        Code::Escape => {
            finished.set(true);
            on_cancel.call(());
        }
        Code::Tab => {
            evt.prevent_default();
            commit(if evt.modifiers().shift() {
                EditNav::Prev
            } else {
                EditNav::Next
            });
        }
        _ => {}
    };
    let focus_on_mount = move |evt: MountedEvent| {
        spawn(async move {
            let _ = evt.set_focus(true).await;
        });
    };
    // Belt and braces for focus: when the editor moves cell-to-cell (Tab),
    // the OLD editor — which holds keyboard focus — unmounts in the same
    // patch that mounts this one, and WebKit's focus fallback (to the body)
    // can land AFTER onmounted's set_focus, leaving the new editor
    // unfocused. Re-focus on the next frame. Only one editor exists at a
    // time, so a fixed element id is unambiguous.
    use_effect(|| {
        document::eval(
            "requestAnimationFrame(() => { \
                const el = document.getElementById('dv-cell-editor'); \
                if (el) el.focus(); \
            });",
        );
    });

    let input_class = "w-full min-w-28 rounded border border-amber-500 bg-slate-950 px-1.5 \
                       py-0.5 font-mono text-xs text-slate-100 outline-none";
    rsx! {
        td { class: "bg-amber-900/30 px-1 py-0.5 align-top",
            div { class: "flex items-start gap-1",
                if kind == EditorKind::Bool {
                    input {
                        r#type: "checkbox",
                        id: "dv-cell-editor",
                        class: "mx-1 my-0.5 accent-amber-500",
                        checked: checked(),
                        onmounted: focus_on_mount,
                        oninput: move |evt| {
                            checked.set(evt.checked());
                            modified.set(true);
                            commit(EditNav::Stay);
                        },
                        onkeydown: on_key,
                        onblur: move |_| commit(EditNav::Stay),
                    }
                } else if multiline {
                    textarea {
                        id: "dv-cell-editor",
                        class: "{input_class} min-w-48",
                        rows: "3",
                        value: "{text}",
                        onmounted: focus_on_mount,
                        oninput: move |evt| {
                            text.set(evt.value());
                            modified.set(true);
                            error.set(None);
                        },
                        onkeydown: on_key,
                        onblur: move |_| commit(EditNav::Stay),
                    }
                } else {
                    input {
                        r#type: "text",
                        id: "dv-cell-editor",
                        class: input_class,
                        value: "{text}",
                        placeholder: if initial.is_null() { "NULL" },
                        onmounted: focus_on_mount,
                        oninput: move |evt| {
                            text.set(evt.value());
                            modified.set(true);
                            error.set(None);
                        },
                        onkeydown: on_key,
                        onblur: move |_| commit(EditNav::Stay),
                    }
                }
                if nullable {
                    button {
                        class: "shrink-0 rounded border border-slate-600 px-1.5 py-0.5 text-xs \
                                text-slate-400 hover:border-amber-500 hover:text-amber-300",
                        title: "Stage NULL (distinct from an empty string)",
                        tabindex: "-1",
                        // prevent_default on mousedown keeps focus in the
                        // input, so its blur-commit cannot race this button.
                        onmousedown: move |evt| evt.prevent_default(),
                        onclick: move |_| {
                            if !finished() {
                                finished.set(true);
                                on_commit.call((Some(Value::Null), EditNav::Stay));
                            }
                        },
                        "∅ NULL"
                    }
                }
            }
            if let Some(message) = error() {
                div { class: "mt-0.5 max-w-md text-xs text-red-400", "{message}" }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn editor_kinds_derive_from_type_substrings() {
        // SQLite declared types (arbitrary case, may carry parens).
        assert_eq!(
            editor_kind("INTEGER"),
            EditorKind::Numeric { decimal: false }
        );
        assert_eq!(editor_kind("VARCHAR(40)"), EditorKind::Text);
        assert_eq!(editor_kind("BOOLEAN"), EditorKind::Bool);
        assert_eq!(editor_kind("BLOB"), EditorKind::Blob);
        assert_eq!(editor_kind("DATETIME"), EditorKind::DateTime);
        assert_eq!(
            editor_kind("DECIMAL(10,5)"),
            EditorKind::Numeric { decimal: true }
        );
        // SQLite columns may have no declared type at all.
        assert_eq!(editor_kind(""), EditorKind::Text);

        // Postgres information_schema data_type strings.
        assert_eq!(
            editor_kind("integer"),
            EditorKind::Numeric { decimal: false }
        );
        assert_eq!(
            editor_kind("smallint"),
            EditorKind::Numeric { decimal: false }
        );
        assert_eq!(
            editor_kind("bigserial"),
            EditorKind::Numeric { decimal: false }
        );
        assert_eq!(
            editor_kind("double precision"),
            EditorKind::Numeric { decimal: false }
        );
        assert_eq!(editor_kind("real"), EditorKind::Numeric { decimal: false });
        assert_eq!(
            editor_kind("numeric"),
            EditorKind::Numeric { decimal: true }
        );
        assert_eq!(editor_kind("boolean"), EditorKind::Bool);
        assert_eq!(editor_kind("json"), EditorKind::Json);
        assert_eq!(editor_kind("jsonb"), EditorKind::Json);
        assert_eq!(editor_kind("date"), EditorKind::DateTime);
        assert_eq!(
            editor_kind("timestamp without time zone"),
            EditorKind::DateTime
        );
        assert_eq!(editor_kind("time with time zone"), EditorKind::DateTime);
        assert_eq!(editor_kind("interval"), EditorKind::DateTime);
        assert_eq!(editor_kind("bytea"), EditorKind::Blob);
        assert_eq!(editor_kind("character varying"), EditorKind::Text);
        assert_eq!(editor_kind("uuid"), EditorKind::Text);
        assert_eq!(editor_kind("USER-DEFINED"), EditorKind::Text);
        // "point" contains "int" but must not get a numeric editor;
        // "interval" contains "int" but is temporal.
        assert_eq!(editor_kind("point"), EditorKind::Text);
    }

    #[test]
    fn numeric_input_stages_integer_real_or_exact_text() {
        let float = |s| parse_numeric(s, false);
        let exact = |s| parse_numeric(s, true);
        assert_eq!(float("42"), Ok(Value::Integer(42)));
        assert_eq!(float(" -7 "), Ok(Value::Integer(-7)));
        assert_eq!(float("1.5"), Ok(Value::Real(1.5)));
        assert_eq!(float("1e3"), Ok(Value::Real(1000.0)));
        // Exact-precision columns: integers still stage as integers, but a
        // fractional value keeps the typed text (no f64 round-trip).
        assert_eq!(exact("42"), Ok(Value::Integer(42)));
        assert_eq!(
            exact("12345678901234567890.123456789"),
            Ok(Value::Text("12345678901234567890.123456789".into()))
        );
        // Rejections: not a number, empty, non-finite.
        assert!(float("abc").is_err());
        assert!(float("").is_err());
        assert!(float("1.2.3").is_err());
        assert!(float("inf").is_err());
        assert!(float("NaN").is_err());
        assert!(exact("12,5").is_err());
    }

    #[test]
    fn json_input_is_validated_and_staged_as_typed_text() {
        let parse = |s| parse_input(EditorKind::Json, Dialect::Postgres, s);
        // Valid JSON keeps the user's exact formatting.
        assert_eq!(
            parse("{\n  \"a\": 1\n}"),
            Ok(Value::Text("{\n  \"a\": 1\n}".into()))
        );
        assert_eq!(parse("[1, 2]"), Ok(Value::Text("[1, 2]".into())));
        assert_eq!(parse("null"), Ok(Value::Text("null".into())));
        let err = parse("{nope").unwrap_err();
        assert!(err.contains("invalid JSON"), "got: {err}");
        assert!(parse("").is_err());
    }

    #[test]
    fn bool_staging_is_dialect_specific() {
        assert_eq!(bool_value(Dialect::Sqlite, true), Value::Integer(1));
        assert_eq!(bool_value(Dialect::Sqlite, false), Value::Integer(0));
        assert_eq!(
            bool_value(Dialect::Postgres, true),
            Value::Text("true".into())
        );
        assert_eq!(
            bool_value(Dialect::Postgres, false),
            Value::Text("false".into())
        );
    }

    #[test]
    fn bool_checked_reads_both_backends_renderings() {
        assert!(bool_checked(&Value::Integer(1)));
        assert!(!bool_checked(&Value::Integer(0)));
        assert!(bool_checked(&Value::Text("true".into())));
        assert!(bool_checked(&Value::Text("TRUE".into())));
        assert!(!bool_checked(&Value::Text("false".into())));
        assert!(!bool_checked(&Value::Text("anything".into())));
        assert!(!bool_checked(&Value::Null));
    }

    #[test]
    fn text_and_datetime_input_stage_as_text_unvalidated() {
        assert_eq!(
            parse_input(EditorKind::Text, Dialect::Sqlite, "hello"),
            Ok(Value::Text("hello".into()))
        );
        // Date/time text is staged as-is; the backend validates on save.
        assert_eq!(
            parse_input(EditorKind::DateTime, Dialect::Postgres, "2024-06-01 12:30"),
            Ok(Value::Text("2024-06-01 12:30".into()))
        );
        assert!(parse_input(EditorKind::Blob, Dialect::Sqlite, "x").is_err());
    }
}
