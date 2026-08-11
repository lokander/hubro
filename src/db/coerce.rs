//! Turning text into a [`Value`] for a typed column — the one vocabulary
//! shared by the inline cell editor (FRE-24) and the file import (FRE-112).
//!
//! Both answer the same question from opposite ends: the editor asks "may I
//! stage what was typed into this cell?", the import asks "may I insert what
//! this field holds into this column?". They classify a declared column type
//! identically ([`classify_type`]) and parse the scalar families identically
//! ([`parse_numeric_text`], [`parse_bool_text`]) — kept here so the two cannot
//! drift into disagreeing about what a `numeric` column accepts.
//!
//! Classification is **case-insensitive substring matching** on the declared
//! type name, in a fixed order, so SQLite declared types ("INTEGER",
//! "VARCHAR(40)", possibly empty) and Postgres/SQL Server `data_type` strings
//! ("integer", "timestamp without time zone", "jsonb") map without a
//! per-backend table.
//!
//! What is *not* shared is the decision each side makes with the answer: the
//! editor renders a widget, the import coerces a field ([`coerce_field`]) and
//! decides whether a bad row is skippable. Nothing here touches the database.

use super::import::{EmptyField, SourceValue};
use super::schema::{ColumnMeta, TypeDetail};
use super::sql::Dialect;
use super::value::Value;

/// The family a declared column type belongs to, derived by
/// [`classify_type`] from the type name alone.
///
/// Deliberately coarse: it distinguishes exactly the cases where a *value*
/// has to be treated differently, and nothing more. Anything unrecognized —
/// including an empty declared type and every user-defined type — is
/// [`TypeClass::Text`], which is the safe direction: text reaches the server
/// verbatim and the server decides.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TypeClass {
    /// Free-form text, and the fallback for unknown declared types.
    Text,
    /// int/serial family: whole numbers only.
    Integer,
    /// real/float/double: any finite number.
    Float,
    /// numeric/decimal: any finite number, carried as text so exact decimals
    /// never round-trip through `f64`. (Not `money`/`smallmoney`, which are
    /// [`Self::Text`] — their literals carry currency symbols and grouping,
    /// so passing them to the server verbatim is the better answer.)
    Exact,
    Bool,
    Json,
    /// date/time/timestamp/interval.
    DateTime,
    /// blob/bytea/binary/varbinary/image.
    Binary,
    /// Postgres `bit`/`bit varying`: a string of `0`/`1`, not a number and
    /// not a boolean (FRE-159). Only ever produced by [`effective_class`],
    /// never by [`classify_type`] — see there for why the distinction cannot
    /// be made from the type name alone.
    BitString,
}

/// Classifies a declared column type. See the module docs for the rules; the
/// **order** of the tests is load-bearing:
///
/// - an empty declared type (legal in SQLite) is text before anything else;
/// - `date`/`time`/`interval` are checked before the numeric families so
///   "interval"'s `int` substring doesn't make it a number;
/// - Postgres `point` is checked for the same reason, and is text.
///
/// SQL Server's binary types (`binary`, `varbinary`, `image`) match on the
/// **base name** — any `(n)`/`(max)` suffix dropped — never by substring,
/// exactly as [`classify_column`](super::page::classify_column) does and for
/// the same reason: a Postgres enum or domain merely *containing* one of
/// those words (an `image_format` enum) is not binary data.
pub fn classify_type(type_name: &str) -> TypeClass {
    let t = type_name.to_ascii_lowercase();
    if t.is_empty() {
        return TypeClass::Text;
    }
    let base = t.split('(').next().unwrap_or("").trim_end();
    if t.contains("blob") || t.contains("bytea") || matches!(base, "binary" | "varbinary" | "image")
    {
        return TypeClass::Binary;
    }
    if t.contains("bool") {
        return TypeClass::Bool;
    }
    if t.contains("json") {
        return TypeClass::Json;
    }
    // Postgres range and multirange types, before the tests they would
    // otherwise trip: `int4range` contains "int", `daterange` contains
    // "date", `tsrange` contains neither — so substring matching split one
    // coherent family three ways and, worse, made an `int4range` column
    // reject `[1,10)`, a literal the server takes happily. A skipped row the
    // server would have accepted is data loss wearing a success message.
    //
    // Text is the right answer for all of them, and the same one
    // [`classify_column`](super::page::classify_column) already gives ranges
    // for the grid's previews.
    if t.contains("range") {
        return TypeClass::Text;
    }
    if t.contains("date") || t.contains("time") || t.contains("interval") {
        return TypeClass::DateTime;
    }
    if t.contains("point") {
        return TypeClass::Text;
    }
    if t.contains("numeric") || t.contains("decimal") {
        return TypeClass::Exact;
    }
    if t.contains("int") || t.contains("serial") {
        return TypeClass::Integer;
    }
    if ["real", "float", "double"].iter().any(|n| t.contains(n)) {
        return TypeClass::Float;
    }
    TypeClass::Text
}

/// [`classify_type`] with the refinements that need to know the backend.
///
/// The word `bit` names two unrelated things, and the name alone cannot tell
/// them apart — which is the whole reason this function exists:
///
/// - **SQL Server `bit` is its boolean type.** Without the refinement the
///   boolean vocabulary was unreachable for the very type [`bool_value`]
///   names: a `yes`/`no` column reached the server as text and failed there —
///   *unskippably*, since only rows hubro rejects itself can be skipped.
/// - **Postgres `bit`/`bit varying` are bit-strings**, where `1010` is a value
///   and `yes` is not. They get [`TypeClass::BitString`], whose check is that
///   the text is `0`s and `1`s (FRE-159).
///
/// `classify_type` — shared with the cell editor, and only ever handed a name
/// — keeps calling both of them text, so neither refinement can leak into a
/// backend it would be wrong for.
///
/// The **declared length is not checked here**, because it is not
/// introspected: `ColumnMeta` carries no `character_maximum_length`, so
/// `bit(4)` and `bit varying(8)` are indistinguishable at this point. A
/// wrong-length value is refused by the server, which says so precisely
/// ("bit string length 2 does not match type bit(4)"). Only the character
/// check is worth making client-side, and it is the one that turns an
/// import-aborting server error into a row that can be skipped.
fn effective_class(column: &ColumnMeta, dialect: Dialect) -> TypeClass {
    let class = classify_type(&column.type_name);
    if class != TypeClass::Text {
        return class;
    }
    if is_bit_string(&column.type_name, dialect) {
        return TypeClass::BitString;
    }
    if dialect == Dialect::SqlServer && column.type_name.trim().eq_ignore_ascii_case("bit") {
        return TypeClass::Bool;
    }
    class
}

/// Whether `type_name` names a Postgres bit-string column.
///
/// The one definition of that rule: [`effective_class`] uses it for the
/// import, and the cell editor's `editor_kind` uses it directly, so the two
/// entry points cannot drift into disagreeing about what a `bit` column is
/// (FRE-159). Dialect-gated rather than name-only, because SQL Server's `bit`
/// is a boolean and must never reach here — see [`effective_class`].
pub(crate) fn is_bit_string(type_name: &str, dialect: Dialect) -> bool {
    dialect == Dialect::Postgres
        && matches!(
            type_name.trim().to_ascii_lowercase().as_str(),
            "bit" | "bit varying"
        )
}

/// Checks a Postgres bit-string literal: `0`s and `1`s, nothing else.
///
/// Shared by the cell editor and the import, because it is literally the same
/// rule — the same arrangement [`parse_numeric_text`] has. Worth doing
/// client-side even though the server would also refuse it: a row the *server*
/// rejects aborts an import on every engine, while one hubro rejects itself
/// can be skipped (see [`super::import`]).
///
/// The declared **length is not checked** — it is not introspected, so `bit(4)`
/// and `bit varying(8)` are indistinguishable here. The server catches a
/// wrong-length value and says so precisely ("bit string length 2 does not
/// match type bit(4)").
///
/// Not trimmed: whitespace is not a bit, and silently accepting `" 101"` would
/// only move the failure to the server.
///
/// The **empty string is accepted**, because `''::bit varying` is a legal
/// zero-length value — refusing it here would stop a column taking something
/// it took before this rule existed. `bit(n)` refuses it server-side, for the
/// same reason it refuses any other wrong length.
pub(crate) fn validate_bit_literal(text: &str) -> Result<(), String> {
    match text.chars().position(|c| c != '0' && c != '1') {
        Some(index) => Err(format!(
            "a bit string takes only 0 and 1 — found {:?} at position {}",
            text.chars().nth(index).unwrap_or('?'),
            index + 1
        )),
        None => Ok(()),
    }
}

/// Parses a number for one of the three numeric classes. Whole numbers are
/// [`Value::Integer`] on every flavor; beyond that:
///
/// - [`TypeClass::Integer`] rejects anything that is not an `i64` — accepting
///   "1.5" for an integer column would silently round through the save-time
///   `::integer` cast;
/// - [`TypeClass::Float`] takes other finite numbers as [`Value::Real`];
/// - [`TypeClass::Exact`] takes them as the typed text, so an exact decimal
///   never round-trips through `f64`.
///
/// Non-numbers, infinities and NaN are rejected everywhere. A class that is
/// not numeric is a caller bug and is rejected too.
pub(crate) fn parse_numeric_text(text: &str, class: TypeClass) -> Result<Value, String> {
    let t = text.trim();
    if t.is_empty() {
        return Err("enter a number (or use the ∅ NULL button)".to_string());
    }
    if let Ok(i) = t.parse::<i64>() {
        return Ok(Value::Integer(i));
    }
    match (class, t.parse::<f64>()) {
        (TypeClass::Integer, Ok(f)) if f.is_finite() => Err(format!("not a whole number: {t}")),
        (TypeClass::Float, Ok(f)) if f.is_finite() => Ok(Value::Real(f)),
        (TypeClass::Exact, Ok(f)) if f.is_finite() => Ok(Value::Text(t.to_string())),
        _ => Err(format!("not a number: {t}")),
    }
}

/// The boolean a piece of text spells, or `None` when it spells neither.
///
/// The *true* vocabulary is also what a fetched cell reads as checked
/// ([`bool_checked`]); the *false* one exists only for the import, which has
/// to tell "this says false" apart from "this says something else entirely"
/// — the editor's checkbox has no third state to report.
pub(crate) fn parse_bool_text(text: &str) -> Option<bool> {
    match text.trim().to_ascii_lowercase().as_str() {
        "true" | "t" | "yes" | "on" | "1" => Some(true),
        "false" | "f" | "no" | "off" | "0" => Some(false),
        _ => None,
    }
}

/// The value a boolean is written as, per backend:
///
/// - SQLite has no boolean storage class — `0`/`1` integers are what its
///   numeric affinity stores natively.
/// - Postgres `boolean` columns reject a bound integer; the text
///   `"true"`/`"false"` is accepted by the `::boolean` cast the staged and
///   imported SQL applies (and by Postgres's own literal parsing).
/// - SQL Server `bit` columns take `0`/`1` (T-SQL has no true/false
///   literals), like SQLite.
pub fn bool_value(dialect: Dialect, checked: bool) -> Value {
    match dialect {
        Dialect::Sqlite | Dialect::SqlServer => Value::Integer(i64::from(checked)),
        Dialect::Postgres => Value::Text(if checked { "true" } else { "false" }.into()),
    }
}

/// Whether a fetched value reads as "checked" when a boolean cell opens.
/// Covers both backends' renderings: SQLite/SQL Server integers and Postgres
/// "true"/"false" text. Anything unrecognized reads as false — a checkbox has
/// nowhere to show a third answer.
pub fn bool_checked(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Integer(i) => *i != 0,
        Value::Real(r) => *r != 0.0,
        Value::Text(t) => parse_bool_text(t) == Some(true),
        Value::Blob(_) => false,
    }
}

/// Coerces one source field into the [`Value`] to insert into `column`
/// (FRE-112), or reports why it cannot — the message names the column and the
/// offending value, since that is all the user has to go on when a row is
/// skipped or an import aborts.
///
/// The rules, in the order they apply:
///
/// - a **missing** field (a JSON object without that key) is NULL. The column
///   is still listed in the INSERT, so a NOT NULL column without a default is
///   rejected by [`super::import`] before the statement is built rather than
///   by the server mid-transaction;
/// - a **JSON null** is NULL — JSON says so outright;
/// - an **empty CSV field** is whatever [`EmptyField`] says, because CSV
///   cannot tell an empty string from a missing value (the export writes both
///   as an empty field — see [`super::export`]) and guessing silently is how
///   an import quietly turns every blank cell into `''` in a NOT NULL column,
///   or into NULL in a column that meant `''`;
/// - a **JSON empty string** is always the empty string. JSON *can* express
///   both, so the option does not apply to it — inventing a NULL where the
///   file said `""` would be discarding information the file actually
///   carried;
/// - an **enum** column takes only its declared variants, so a typo is caught
///   here instead of aborting the transaction server-side;
/// - otherwise the column's [`TypeClass`] decides.
///
/// Where a class has no client-side check worth making — dates, arrays and
/// user-defined types — the text is passed through and the server validates
/// it. That is a real limit on skip mode, stated in [`super::import`]: a row
/// the *server* rejects aborts the import on every engine, because only the
/// rows hubro rejected itself can be skipped without the transaction already
/// being in trouble.
pub(crate) fn coerce_field(
    column: &ColumnMeta,
    dialect: Dialect,
    field: &SourceValue,
    empty: EmptyField,
) -> Result<Value, String> {
    let class = effective_class(column, dialect);
    let text = match field {
        SourceValue::Missing => return Ok(Value::Null),
        SourceValue::Json(serde_json::Value::Null) => return Ok(Value::Null),
        SourceValue::Text(text) => {
            if text.is_empty() && empty == EmptyField::Null {
                return Ok(Value::Null);
            }
            text.clone()
        }
        SourceValue::Json(value) => return coerce_json(column, dialect, class, value),
    };
    coerce_text(column, dialect, class, &text)
}

/// [`coerce_field`]'s text arm: a CSV field, or a JSON string, against the
/// column's class.
fn coerce_text(
    column: &ColumnMeta,
    dialect: Dialect,
    class: TypeClass,
    text: &str,
) -> Result<Value, String> {
    if let TypeDetail::Enum { variants, .. } = &column.type_detail {
        if !variants.is_empty() && !variants.iter().any(|v| v == text) {
            return Err(format!(
                "column \"{}\": {text:?} is not one of its values ({})",
                column.name,
                variants.join(", ")
            ));
        }
        return Ok(Value::Text(text.to_string()));
    }
    // An empty value only means something for a textual column. Reaching
    // here with one means either [`EmptyField::EmptyText`] or a JSON `""`,
    // and there is no empty number, boolean or blob — so it is refused by
    // name, pointing at the option that would make it NULL instead. Silently
    // treating it as NULL anyway would make the option a lie in exactly the
    // place someone would go looking for it.
    // Trimmed, so a whitespace-only field takes this import-aware message
    // rather than falling through to the numeric parser's — whose wording
    // ("use the ∅ NULL button") belongs to the cell editor and names a
    // button this dialog does not have.
    // `BitString` joins Text in the exemption: the empty string is a legal
    // zero-length `bit varying` value, so refusing it by name here would take
    // away something the column accepts (see [`validate_bit_literal`]).
    if text.trim().is_empty() && !matches!(class, TypeClass::Text | TypeClass::BitString) {
        return Err(format!(
            "column \"{}\": an empty value is not {} — choose \"{}\" if it should be NULL",
            column.name,
            match class {
                TypeClass::Integer | TypeClass::Float | TypeClass::Exact => "a number",
                TypeClass::Bool => "a true/false value",
                TypeClass::Json => "JSON",
                TypeClass::DateTime => "a date or time",
                TypeClass::Binary => "binary data",
                // Unreachable: the guard above exempts both of these, being
                // the classes an empty value means something for. Named
                // rather than folded into an arm they are not, so this does
                // not read as a decision someone made.
                TypeClass::Text | TypeClass::BitString => {
                    unreachable!("Text and BitString are exempted above")
                }
            },
            EmptyField::Null.label(),
        ));
    }
    match class {
        // Text, dates and everything unrecognized go to the server verbatim.
        TypeClass::Text | TypeClass::DateTime => Ok(Value::Text(text.to_string())),
        TypeClass::Integer | TypeClass::Float | TypeClass::Exact => parse_numeric_text(text, class)
            .map_err(|why| format!("column \"{}\": {why}", column.name)),
        TypeClass::Bool => match parse_bool_text(text) {
            Some(flag) => Ok(bool_value(dialect, flag)),
            None => Err(format!(
                "column \"{}\": {text:?} is not a true/false value",
                column.name
            )),
        },
        TypeClass::Json => match serde_json::from_str::<serde_json::Value>(text) {
            Ok(_) => Ok(Value::Text(text.to_string())),
            Err(err) => Err(format!("column \"{}\": invalid JSON ({err})", column.name)),
        },
        TypeClass::BitString => validate_bit_literal(text)
            .map(|()| Value::Text(text.to_string()))
            .map_err(|why| format!("column \"{}\": {why}", column.name)),
        TypeClass::Binary => parse_hex(text).map(Value::Blob).ok_or_else(|| {
            format!(
                "column \"{}\": {} is not hex — a binary column takes the \\x-prefixed form the \
                 CSV/JSON export writes",
                column.name,
                elide(text)
            )
        }),
    }
}

/// [`coerce_field`]'s JSON arm: a typed JSON value against the column's
/// class. Numbers and booleans arrive already typed, so they are used
/// directly where the column agrees and rendered as text where the column is
/// textual; objects and arrays are re-serialized, which only a JSON column
/// can take.
fn coerce_json(
    column: &ColumnMeta,
    dialect: Dialect,
    class: TypeClass,
    value: &serde_json::Value,
) -> Result<Value, String> {
    match value {
        serde_json::Value::String(text) => coerce_text(column, dialect, class, text),
        serde_json::Value::Bool(flag) => match class {
            TypeClass::Bool => Ok(bool_value(dialect, *flag)),
            TypeClass::Text | TypeClass::Json => Ok(Value::Text(flag.to_string())),
            TypeClass::Integer => Ok(Value::Integer(i64::from(*flag))),
            _ => Err(format!(
                "column \"{}\": {flag} is not a value this column can hold",
                column.name
            )),
        },
        serde_json::Value::Number(number) => match class {
            TypeClass::Bool => Err(format!(
                "column \"{}\": {number} is not a true/false value",
                column.name
            )),
            TypeClass::Binary => Err(format!(
                "column \"{}\": {number} is not binary data",
                column.name
            )),
            // Rendering the number back to text and parsing it under the
            // column's own rules keeps ONE definition of "may this number go
            // in this column" — an integer column rejects 1.5 identically
            // whether it arrived from CSV or from JSON.
            _ => coerce_text(column, dialect, class, &number.to_string()),
        },
        serde_json::Value::Array(_) | serde_json::Value::Object(_) => match class {
            TypeClass::Json | TypeClass::Text => Ok(Value::Text(value.to_string())),
            _ => Err(format!(
                "column \"{}\": {} is a nested JSON value, which this column can't hold",
                column.name,
                elide(&value.to_string())
            )),
        },
        // Handled by `coerce_field` before it gets here.
        serde_json::Value::Null => Ok(Value::Null),
    }
}

/// Decodes the `\x`-prefixed hex a blob exports as (see
/// [`export::hex_literal`](super::export)) back into bytes. The prefix is
/// required: without it a plain string of digits would be ambiguous between
/// hex and text, and silently importing `1234` as two bytes would be worse
/// than refusing it.
fn parse_hex(text: &str) -> Option<Vec<u8>> {
    let digits = text
        .strip_prefix("\\x")
        .or_else(|| text.strip_prefix("\\X"))?;
    if digits.len() % 2 != 0 {
        return None;
    }
    let bytes = digits.as_bytes();
    let mut out = Vec::with_capacity(digits.len() / 2);
    for pair in bytes.chunks(2) {
        let hi = (pair[0] as char).to_digit(16)?;
        let lo = (pair[1] as char).to_digit(16)?;
        out.push((hi * 16 + lo) as u8);
    }
    Some(out)
}

/// A value quoted for an error message, shortened so one enormous cell can't
/// turn a per-row message into a wall of text.
fn elide(text: &str) -> String {
    const MAX: usize = 60;
    if text.chars().count() <= MAX {
        return format!("{text:?}");
    }
    let head: String = text.chars().take(MAX).collect();
    format!("{head:?}…")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::import::{EmptyField, SourceValue};
    use crate::db::schema::{Generated, TypeRef};

    fn column(name: &str, type_name: &str) -> ColumnMeta {
        ColumnMeta {
            name: name.into(),
            type_name: type_name.into(),
            nullable: true,
            primary_key_position: None,
            default: None,
            generated: Generated::Never,
            type_detail: TypeDetail::Plain,
        }
    }

    fn text(value: &str) -> SourceValue {
        SourceValue::Text(value.to_string())
    }

    #[test]
    fn bit_means_two_different_things_and_the_dialect_decides_which() {
        // The distinction FRE-112 introduced this function for, now carrying a
        // second case (FRE-159). Conflating them is not a cosmetic error:
        // reading a Postgres bit-string as a boolean would turn "1010" into
        // `true`, and reading SQL Server's bit as a bit-string would reject
        // every "yes"/"no" the boolean vocabulary exists to accept.
        assert_eq!(
            effective_class(&column("mask", "bit"), Dialect::Postgres),
            TypeClass::BitString
        );
        assert_eq!(
            effective_class(&column("flags", "bit varying"), Dialect::Postgres),
            TypeClass::BitString
        );
        assert_eq!(
            effective_class(&column("active", "bit"), Dialect::SqlServer),
            TypeClass::Bool
        );
        // SQLite has neither; the name means nothing there.
        assert_eq!(
            effective_class(&column("mask", "bit"), Dialect::Sqlite),
            TypeClass::Text
        );

        // `classify_type` is name-only and shared with the cell editor, so it
        // must never produce either refinement on its own.
        assert_eq!(classify_type("bit"), TypeClass::Text);
        assert_eq!(classify_type("bit varying"), TypeClass::Text);

        // The shared predicate agrees with the class, and is dialect-gated.
        assert!(is_bit_string("bit", Dialect::Postgres));
        assert!(is_bit_string("BIT VARYING", Dialect::Postgres));
        assert!(!is_bit_string("bit", Dialect::SqlServer));
        assert!(!is_bit_string("bigint", Dialect::Postgres));
    }

    #[test]
    fn a_bit_string_takes_only_zeroes_and_ones() {
        assert!(validate_bit_literal("1010").is_ok());
        assert!(validate_bit_literal("0").is_ok());
        // Rejected client-side so an import can *skip* the row: a value the
        // server refuses aborts the whole transaction instead.
        assert!(validate_bit_literal("1012").is_err());
        assert!(validate_bit_literal("yes").is_err());
        // The empty string is a legal zero-length `bit varying` value, so it
        // passes here — refusing it would take away something the column
        // accepts, and `bit(n)` refuses it server-side like any wrong length.
        assert!(validate_bit_literal("").is_ok());
        // Not trimmed — whitespace is not a bit, and accepting it here would
        // only move the failure to the server.
        assert!(validate_bit_literal(" 101").is_err());
        // The message points at the offending character, since "invalid" on
        // its own does not help someone staring at a long mask.
        let why = validate_bit_literal("1102").unwrap_err();
        assert!(why.contains("'2'"), "{why}");
        assert!(why.contains("position 4"), "{why}");

        // The import reaches the same rule through the column's class.
        let err = coerce_field(
            &column("mask", "bit"),
            Dialect::Postgres,
            &text("10x0"),
            EmptyField::EmptyText,
        )
        .unwrap_err();
        assert!(err.contains("mask"), "the column must be named: {err}");
        assert!(err.contains("only 0 and 1"), "{err}");

        // An empty CSV field must reach the server as `''` — a legal
        // zero-length `bit varying` value. It gets there only because
        // `BitString` shares Text's exemption from the empty-value guard, so
        // this is asserted through `coerce_field` rather than through the
        // validator alone: reverting that exemption refuses the field with
        // "an empty value is not a bit string" and never reaches the
        // validator at all.
        assert_eq!(
            coerce_field(
                &column("mask", "bit"),
                Dialect::Postgres,
                &text(""),
                EmptyField::EmptyText
            )
            .unwrap(),
            Value::Text(String::new())
        );
        // …while the option that says an empty field means NULL still wins.
        assert_eq!(
            coerce_field(
                &column("mask", "bit"),
                Dialect::Postgres,
                &text(""),
                EmptyField::Null
            )
            .unwrap(),
            Value::Null
        );

        // A good one passes through as text — the `::bit varying` cast the
        // statement carries is what types it.
        assert_eq!(
            coerce_field(
                &column("mask", "bit"),
                Dialect::Postgres,
                &text("1010"),
                EmptyField::EmptyText
            )
            .unwrap(),
            Value::Text("1010".into())
        );
    }

    fn json(raw: &str) -> SourceValue {
        SourceValue::Json(serde_json::from_str(raw).unwrap())
    }

    fn coerce(type_name: &str, field: &SourceValue) -> Result<Value, String> {
        coerce_field(
            &column("c", type_name),
            Dialect::Postgres,
            field,
            EmptyField::Null,
        )
    }

    #[test]
    fn declared_types_classify_by_family() {
        for (type_name, expected) in [
            ("", TypeClass::Text),
            ("TEXT", TypeClass::Text),
            ("character varying", TypeClass::Text),
            ("integer", TypeClass::Integer),
            ("BIGINT UNSIGNED", TypeClass::Integer),
            ("serial", TypeClass::Integer),
            ("double precision", TypeClass::Float),
            ("REAL", TypeClass::Float),
            ("numeric", TypeClass::Exact),
            ("decimal(10,2)", TypeClass::Exact),
            ("boolean", TypeClass::Bool),
            ("bit", TypeClass::Text),
            ("jsonb", TypeClass::Json),
            ("timestamp without time zone", TypeClass::DateTime),
            ("interval", TypeClass::DateTime),
            ("date", TypeClass::DateTime),
            ("bytea", TypeClass::Binary),
            ("varbinary(max)", TypeClass::Binary),
            ("BLOB", TypeClass::Binary),
            // Postgres geometric point: contains "int", is not a number.
            ("point", TypeClass::Text),
            // A user type merely containing a binary type's name is not
            // binary — the reason those match by base name.
            ("image_format", TypeClass::Text),
            ("binary_state", TypeClass::Text),
            // Unrecognized user types are text...
            ("app.shape", TypeClass::Text),
            // ...except where a family's substring happens to occur inside
            // the name, which is a known cost of the rule rather than an
            // intent. Pinned so the wart is recorded rather than
            // rediscovered: it predates the import and is what the cell
            // editor has always done with such a column.
            ("app.fingerprint", TypeClass::Integer),
            ("app.timeline", TypeClass::DateTime),
        ] {
            assert_eq!(classify_type(type_name), expected, "{type_name}");
        }
    }

    #[test]
    fn postgres_ranges_pass_through_instead_of_being_read_as_numbers() {
        // `int4range` contains "int": read as an integer column it rejected
        // `[1,10)`, a literal the server accepts — a row silently dropped in
        // skip mode, which is data loss wearing a success message. The whole
        // family answers the same way now, where before it split three ways.
        for type_name in [
            "int4range",
            "int8range",
            "numrange",
            "tsrange",
            "tstzrange",
            "daterange",
            "int4multirange",
        ] {
            assert_eq!(classify_type(type_name), TypeClass::Text, "{type_name}");
            assert_eq!(
                coerce(type_name, &text("[1,10)")),
                Ok(Value::Text("[1,10)".into())),
                "{type_name}"
            );
        }
    }

    #[test]
    fn sql_server_bit_is_boolean_but_postgres_bit_is_a_bit_string() {
        // The one classification that has to know the backend: `bit` names
        // two unrelated types. On SQL Server the boolean vocabulary applies
        // (and `bool_value`'s SqlServer arm is finally reachable)...
        let column = column("active", "bit");
        assert_eq!(
            coerce_field(&column, Dialect::SqlServer, &text("yes"), EmptyField::Null),
            Ok(Value::Integer(1))
        );
        assert_eq!(
            coerce_field(&column, Dialect::SqlServer, &text("no"), EmptyField::Null),
            Ok(Value::Integer(0))
        );
        // ...and a value that is neither is refused here, where skip mode can
        // still skip it, rather than by the server mid-transaction.
        assert!(coerce_field(&column, Dialect::SqlServer, &text("2"), EmptyField::Null).is_err());

        // On Postgres the same name is a bit-string: `1010` is a value and
        // `yes` is not, so it stays text and the server judges it.
        assert_eq!(
            coerce_field(&column, Dialect::Postgres, &text("1010"), EmptyField::Null),
            Ok(Value::Text("1010".into()))
        );
        assert_eq!(classify_type("bit"), TypeClass::Text);
        assert_eq!(classify_type("bit varying"), TypeClass::Text);
    }

    #[test]
    fn a_whitespace_only_field_gets_the_imports_own_message() {
        // Not the cell editor's "use the ∅ NULL button", which names a
        // button the import dialog does not have.
        let err = coerce("integer", &text("   ")).unwrap_err();
        assert!(!err.contains('∅'), "{err}");
        assert!(err.contains("empty value is not a number"), "{err}");
        assert!(err.contains(EmptyField::Null.label()), "{err}");
    }

    #[test]
    fn an_empty_csv_field_follows_the_explicit_option() {
        let column = column("c", "text");
        assert_eq!(
            coerce_field(&column, Dialect::Postgres, &text(""), EmptyField::Null),
            Ok(Value::Null)
        );
        assert_eq!(
            coerce_field(&column, Dialect::Postgres, &text(""), EmptyField::EmptyText),
            Ok(Value::Text(String::new()))
        );
    }

    #[test]
    fn json_keeps_its_own_null_and_empty_string_apart() {
        let column = column("c", "text");
        // The option is a CSV rule: JSON says which it means, both ways, and
        // under either setting.
        for empty in [EmptyField::Null, EmptyField::EmptyText] {
            assert_eq!(
                coerce_field(&column, Dialect::Postgres, &json("null"), empty),
                Ok(Value::Null)
            );
            assert_eq!(
                coerce_field(&column, Dialect::Postgres, &json("\"\""), empty),
                Ok(Value::Text(String::new()))
            );
        }
    }

    #[test]
    fn a_missing_field_is_null() {
        assert_eq!(coerce("text", &SourceValue::Missing), Ok(Value::Null));
    }

    #[test]
    fn numbers_are_checked_against_the_column_family() {
        assert_eq!(coerce("integer", &text("42")), Ok(Value::Integer(42)));
        assert_eq!(coerce("integer", &text(" 42 ")), Ok(Value::Integer(42)));
        assert_eq!(
            coerce("double precision", &text("1.5")),
            Ok(Value::Real(1.5))
        );
        // An exact decimal never round-trips through f64.
        assert_eq!(
            coerce("numeric", &text("1.10")),
            Ok(Value::Text("1.10".into()))
        );

        // The rejection an import has to report clearly.
        let err = coerce("integer", &text("abc")).unwrap_err();
        assert!(err.contains("\"c\""), "{err}");
        assert!(err.contains("abc"), "{err}");
        // Fractional input for an integer column is rejected, not rounded.
        assert!(coerce("integer", &text("1.5")).is_err());
        assert!(coerce("double precision", &text("NaN")).is_err());
    }

    #[test]
    fn a_json_number_obeys_the_same_column_rules_as_csv_text() {
        assert_eq!(coerce("integer", &json("42")), Ok(Value::Integer(42)));
        assert!(coerce("integer", &json("1.5")).is_err());
        assert_eq!(coerce("text", &json("42")), Ok(Value::Text("42".into())));
    }

    #[test]
    fn booleans_take_the_spellings_people_export() {
        for (spelling, expected) in [
            ("true", true),
            ("TRUE", true),
            ("t", true),
            ("yes", true),
            ("1", true),
            ("false", false),
            ("f", false),
            ("no", false),
            ("0", false),
        ] {
            assert_eq!(
                coerce("boolean", &text(spelling)),
                Ok(bool_value(Dialect::Postgres, expected)),
                "{spelling}"
            );
        }
        assert_eq!(
            coerce("boolean", &json("true")),
            Ok(Value::Text("true".into()))
        );
        let err = coerce("boolean", &text("maybe")).unwrap_err();
        assert!(err.contains("true/false"), "{err}");
    }

    #[test]
    fn bool_value_and_bool_checked_round_trip_per_dialect() {
        for dialect in [Dialect::Sqlite, Dialect::Postgres, Dialect::SqlServer] {
            for flag in [true, false] {
                assert_eq!(
                    bool_checked(&bool_value(dialect, flag)),
                    flag,
                    "{dialect:?}"
                );
            }
        }
        assert!(!bool_checked(&Value::Text("maybe".into())));
        assert!(!bool_checked(&Value::Null));
    }

    #[test]
    fn json_columns_take_scalars_objects_and_arrays() {
        assert_eq!(
            coerce("jsonb", &json(r#"{"a":1}"#)),
            Ok(Value::Text(r#"{"a":1}"#.into()))
        );
        assert_eq!(
            coerce("jsonb", &text(r#"[1,2]"#)),
            Ok(Value::Text("[1,2]".into()))
        );
        // A nested value has nowhere to go in a scalar column.
        let err = coerce("integer", &json(r#"{"a":1}"#)).unwrap_err();
        assert!(err.contains("nested"), "{err}");
        assert!(coerce("jsonb", &text("{oops")).is_err());
    }

    #[test]
    fn binary_columns_take_the_hex_the_export_writes() {
        assert_eq!(
            coerce("bytea", &text("\\x00ff10")),
            Ok(Value::Blob(vec![0x00, 0xff, 0x10]))
        );
        // Unprefixed digits are refused rather than silently read as hex.
        let err = coerce("bytea", &text("00ff10")).unwrap_err();
        assert!(err.contains("hex"), "{err}");
        assert!(coerce("bytea", &text("\\x0")).is_err());
        assert!(coerce("bytea", &text("\\xzz")).is_err());
    }

    #[test]
    fn an_enum_column_takes_only_its_declared_variants() {
        let mut column = column("mood", "USER-DEFINED");
        column.type_detail = TypeDetail::Enum {
            type_ref: TypeRef {
                schema: "public".into(),
                name: "mood".into(),
            },
            variants: vec!["happy".into(), "sad".into()],
        };
        assert_eq!(
            coerce_field(&column, Dialect::Postgres, &text("happy"), EmptyField::Null),
            Ok(Value::Text("happy".into()))
        );
        let err = coerce_field(
            &column,
            Dialect::Postgres,
            &text("elated"),
            EmptyField::Null,
        )
        .unwrap_err();
        assert!(err.contains("elated"), "{err}");
        assert!(err.contains("happy, sad"), "{err}");
    }

    #[test]
    fn dates_and_unknown_types_pass_through_for_the_server_to_judge() {
        assert_eq!(
            coerce("timestamp without time zone", &text("2024-06-01 12:30")),
            Ok(Value::Text("2024-06-01 12:30".into()))
        );
        // Deliberately NOT validated here — see `coerce_field`'s docs.
        assert_eq!(
            coerce("date", &text("not a date")),
            Ok(Value::Text("not a date".into()))
        );
    }

    #[test]
    fn an_overlong_value_is_elided_in_the_message() {
        let long = "x".repeat(500);
        let err = coerce("bytea", &text(&long)).unwrap_err();
        assert!(err.len() < 200, "message should stay readable: {err}");
        assert!(err.contains('…'), "{err}");
    }
}
