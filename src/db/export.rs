//! Streaming CSV/JSON export of query results.
//!
//! Two entry points share one set of formatters:
//!
//! - [`DbPool::export`](super::DbPool::export) streams a live query, pulling
//!   rows from sqlx ONE AT A TIME (`fetch`, not `fetch_all`) and writing each
//!   incrementally. Peak memory is one decoded row plus the writer's buffer,
//!   regardless of result size — a million-row export never materializes.
//! - [`write_result`] serializes an already-materialized [`QueryResult`] (the
//!   SQL editor's held result), reusing the exact same per-row formatting.
//!
//! ## Encoding choices
//!
//! CSV follows RFC-4180 quoting with LF (`\n`) line terminators: a field is
//! wrapped in double quotes only when it contains a comma, a double quote, or
//! a CR/LF, and embedded quotes are doubled. Values render as `NULL` → empty
//! field, integers/reals → their plain decimal form, text verbatim, blobs →
//! a `\x`-prefixed hex string — each streamed straight from the borrowed
//! value, never materialized as a per-cell `String` (FRE-132; on a 1M×10
//! export the old pipeline made ~10M transient allocations). Because `NULL`
//! and the empty string both render to an empty field, CSV cannot
//! distinguish them — a documented, standard CSV limitation. Text values
//! starting with `=` `+` `-` `@` are prefixed with an apostrophe so
//! spreadsheet apps don't execute them as formulas (see
//! [`needs_formula_hardening`]).
//!
//! JSON is a top-level array of objects keyed by column name, streamed
//! element by element. `NULL` → `null`, integers/reals → JSON numbers (a
//! non-finite real, which JSON cannot represent, degrades to its string form
//! e.g. `"NaN"`), text → JSON strings, blobs → the same `\x`-prefixed hex
//! string. An empty result is `[]`. Keys keep column order (not sorted).

use std::io::{self, Write};

use super::error::DbError;
use super::value::{QueryResult, Value};

/// Wraps a writer I/O failure (e.g. disk full while streaming an export) as a
/// [`DbError`] so the export task can surface it like any other failure.
pub(crate) fn export_io_err(err: io::Error) -> DbError {
    DbError::Query(format!("writing export: {err}"))
}

/// The two export encodings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExportFormat {
    Csv,
    Json,
}

impl ExportFormat {
    /// Default file extension (drives the save dialog's suggested name).
    pub fn extension(self) -> &'static str {
        match self {
            ExportFormat::Csv => "csv",
            ExportFormat::Json => "json",
        }
    }
}

/// Incremental writer shared by the streaming and materialized paths. Holds
/// the column names and the format's running state (JSON needs to know
/// whether it has already emitted an element); values are handed in one row
/// at a time so nothing accumulates.
pub struct ExportSink {
    format: ExportFormat,
    columns: Vec<String>,
    wrote_row: bool,
}

impl ExportSink {
    pub fn new(format: ExportFormat, columns: Vec<String>) -> Self {
        ExportSink {
            format,
            columns,
            wrote_row: false,
        }
    }

    /// Writes the preamble: the CSV header row, or the JSON `[`.
    pub fn begin(&mut self, out: &mut impl Write) -> io::Result<()> {
        match self.format {
            ExportFormat::Csv => write_csv_record(self.columns.iter().map(String::as_str), out),
            ExportFormat::Json => out.write_all(b"["),
        }
    }

    /// Writes one data row.
    pub fn write_row(&mut self, row: &[Value], out: &mut impl Write) -> io::Result<()> {
        match self.format {
            ExportFormat::Csv => {
                for (idx, value) in row.iter().enumerate() {
                    if idx > 0 {
                        out.write_all(b",")?;
                    }
                    write_csv_value(value, &mut *out)?;
                }
                out.write_all(b"\n")
            }
            ExportFormat::Json => {
                out.write_all(if self.wrote_row { b",\n  " } else { b"\n  " })?;
                write_json_object(&self.columns, row, out)?;
                self.wrote_row = true;
                Ok(())
            }
        }
    }

    /// Writes the postamble: nothing for CSV, the closing `]` for JSON.
    pub fn end(&mut self, out: &mut impl Write) -> io::Result<()> {
        match self.format {
            ExportFormat::Csv => Ok(()),
            ExportFormat::Json => {
                if self.wrote_row {
                    out.write_all(b"\n]\n")
                } else {
                    out.write_all(b"]\n")
                }
            }
        }
    }
}

/// Serializes a materialized [`QueryResult`], returning the number of data
/// rows written. Used by the SQL editor's export (the result is already in
/// memory) and by the formatter unit tests.
pub fn write_result(
    result: &QueryResult,
    format: ExportFormat,
    out: &mut impl Write,
) -> io::Result<u64> {
    let columns = result.columns.iter().map(|c| c.name.clone()).collect();
    let mut sink = ExportSink::new(format, columns);
    sink.begin(out)?;
    let mut rows = 0u64;
    for row in &result.rows {
        sink.write_row(row, out)?;
        rows += 1;
    }
    sink.end(out)?;
    Ok(rows)
}

/// Writes one CSV record (a sequence of string fields) followed by an LF,
/// quoting each field per RFC-4180. Used for the header row; data rows stream
/// each cell through [`write_csv_value`] instead.
fn write_csv_record<'a, I>(fields: I, out: &mut impl Write) -> io::Result<()>
where
    I: IntoIterator<Item = &'a str>,
{
    let mut first = true;
    for field in fields {
        if !first {
            out.write_all(b",")?;
        }
        first = false;
        write_csv_field(field, out)?;
    }
    out.write_all(b"\n")
}

/// Writes one CSV field, wrapping it in quotes (and doubling embedded quotes)
/// only when it contains a delimiter, a quote, or a line break.
fn write_csv_field(field: &str, out: &mut impl Write) -> io::Result<()> {
    if field.contains([',', '"', '\n', '\r']) {
        out.write_all(b"\"")?;
        write_quote_doubled(field, out)?;
        out.write_all(b"\"")
    } else {
        out.write_all(field.as_bytes())
    }
}

/// Streams `field` with every embedded `"` doubled, writing the borrowed
/// slices between quotes directly — the one place the CSV path re-walks a
/// string, and it still never copies it.
fn write_quote_doubled(field: &str, out: &mut impl Write) -> io::Result<()> {
    let mut rest = field;
    while let Some(pos) = rest.find('"') {
        let (before, after) = rest.split_at(pos);
        out.write_all(before.as_bytes())?;
        out.write_all(b"\"\"")?;
        rest = &after[1..];
    }
    out.write_all(rest.as_bytes())
}

/// Streams one cell as a CSV field, allocation-free (FRE-132): numbers and
/// blob hex go straight through the formatter — neither can contain a
/// character on the RFC-4180 quoting list, so they never need quoting — and
/// text is written borrowed by [`write_csv_text`].
///
/// The clipboard's delimited formats (FRE-110) mirror this arm for arm except
/// for NULL, which they must keep distinct from the empty string — see
/// [`super::clipboard`].
fn write_csv_value(value: &Value, out: &mut impl Write) -> io::Result<()> {
    match value {
        Value::Null => Ok(()),
        Value::Integer(i) => write!(out, "{i}"),
        Value::Real(r) => write!(out, "{r}"),
        Value::Text(t) => write_csv_text(t, out),
        // Byte-by-byte hex, same bytes as [`hex_literal`] without the String.
        Value::Blob(b) => {
            out.write_all(b"\\x")?;
            for byte in b {
                write!(out, "{byte:02x}")?;
            }
            Ok(())
        }
    }
}

/// Writes one text cell: decides quoting by scanning the borrowed `&str`,
/// then writes the formula-hardening apostrophe and the text directly. The
/// apostrophe is not on the RFC-4180 quoting list, so scanning the original
/// text answers the quoting question for the hardened form too — and when
/// quoting does apply, the apostrophe lands inside the quotes, exactly where
/// quoting the hardened copy used to put it.
fn write_csv_text(text: &str, out: &mut impl Write) -> io::Result<()> {
    let quote = text.contains([',', '"', '\n', '\r']);
    if quote {
        out.write_all(b"\"")?;
    }
    if needs_formula_hardening(text) {
        out.write_all(b"'")?;
    }
    if quote {
        write_quote_doubled(text, out)?;
        out.write_all(b"\"")
    } else {
        out.write_all(text.as_bytes())
    }
}

/// Whether a text value triggers CSV formula injection (FRE-73): Excel and
/// LibreOffice execute cells starting with `=` `+` `-` `@` (or a tab, per
/// OWASP's list) as live formulas on open, so such a value gets the standard
/// leading-apostrophe prefix. Text only — numbers render bare (`-7` must
/// stay a number) and blobs are `\x`-prefixed; JSON is not an executable
/// format and stays verbatim.
///
/// The character list lives here alone so the streaming export
/// ([`write_csv_text`]) and the clipboard's owned-string path
/// ([`harden_csv_text`]) cannot drift apart.
fn needs_formula_hardening(text: &str) -> bool {
    text.starts_with(['=', '+', '-', '@', '\t'])
}

/// Owned-string form of the formula hardening, for the clipboard formats
/// (FRE-110), which build each cell as a `String` anyway: pasting text into a
/// spreadsheet runs the same formulas that opening a file does. The streaming
/// CSV export applies the identical rule without allocating — see
/// [`write_csv_text`].
pub(crate) fn harden_csv_text(text: &str) -> String {
    if needs_formula_hardening(text) {
        format!("'{text}")
    } else {
        text.to_string()
    }
}

/// Writes a JSON object for one row, keyed by column name in column order.
/// A row with more values than column names still emits every value under a
/// positional `"col{n}"` key (defensive; column/row length always match in
/// practice).
fn write_json_object(columns: &[String], row: &[Value], out: &mut impl Write) -> io::Result<()> {
    out.write_all(b"{")?;
    for (idx, value) in row.iter().enumerate() {
        if idx > 0 {
            out.write_all(b",")?;
        }
        let fallback;
        let key = match columns.get(idx) {
            Some(name) => name.as_str(),
            None => {
                fallback = format!("col{idx}");
                fallback.as_str()
            }
        };
        serde_json::to_writer(&mut *out, key)?;
        out.write_all(b":")?;
        write_json_value(value, out)?;
    }
    out.write_all(b"}")
}

/// Writes one cell as a JSON value.
fn write_json_value(value: &Value, out: &mut impl Write) -> io::Result<()> {
    match value {
        Value::Null => out.write_all(b"null"),
        Value::Integer(i) => write!(out, "{i}"),
        // A finite real serializes as a JSON number; NaN/Infinity (which JSON
        // forbids) degrade to their string form so the export stays valid.
        Value::Real(r) if r.is_finite() => {
            serde_json::to_writer(&mut *out, r).map_err(io::Error::from)
        }
        Value::Real(r) => serde_json::to_writer(&mut *out, &r.to_string()).map_err(io::Error::from),
        Value::Text(t) => serde_json::to_writer(&mut *out, t).map_err(io::Error::from),
        Value::Blob(b) => {
            serde_json::to_writer(&mut *out, &hex_literal(b)).map_err(io::Error::from)
        }
    }
}

/// Postgres-style `\x…` hex rendering of a blob, shared by both formats — and
/// by the clipboard ones (FRE-110) — so a round-trip is unambiguous.
pub(crate) fn hex_literal(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(2 + bytes.len() * 2);
    s.push_str("\\x");
    for byte in bytes {
        use std::fmt::Write as _;
        let _ = write!(s, "{byte:02x}");
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::ColumnInfo;

    fn result(columns: &[&str], rows: Vec<Vec<Value>>) -> QueryResult {
        QueryResult {
            columns: columns
                .iter()
                .map(|c| ColumnInfo {
                    name: (*c).to_string(),
                })
                .collect(),
            rows,
        }
    }

    fn to_string(result: &QueryResult, format: ExportFormat) -> String {
        let mut buf = Vec::new();
        write_result(result, format, &mut buf).unwrap();
        String::from_utf8(buf).unwrap()
    }

    #[test]
    fn csv_quotes_only_fields_that_need_it() {
        let r = result(
            &["a", "b", "c"],
            vec![vec![
                Value::Text("plain".into()),
                Value::Text("has,comma".into()),
                Value::Text("has\"quote".into()),
            ]],
        );
        assert_eq!(
            to_string(&r, ExportFormat::Csv),
            "a,b,c\nplain,\"has,comma\",\"has\"\"quote\"\n"
        );
    }

    #[test]
    fn csv_quotes_newlines_and_carriage_returns() {
        let r = result(&["a"], vec![vec![Value::Text("line1\nline2\r".into())]]);
        assert_eq!(to_string(&r, ExportFormat::Csv), "a\n\"line1\nline2\r\"\n");
    }

    #[test]
    fn csv_renders_types_null_and_blob() {
        let r = result(
            &["n", "i", "r", "b"],
            vec![vec![
                Value::Null,
                Value::Integer(-7),
                Value::Real(1.5),
                Value::Blob(vec![0x00, 0xff, 0x10]),
            ]],
        );
        // NULL is an empty field; the blob is an unquoted hex literal.
        assert_eq!(
            to_string(&r, ExportFormat::Csv),
            "n,i,r,b\n,-7,1.5,\\x00ff10\n"
        );
    }

    #[test]
    fn csv_neutralizes_formula_prefixes_in_text_only() {
        let r = result(
            &["a", "b", "c", "d", "e"],
            vec![vec![
                Value::Text("=cmd|'/C calc'!A0".into()),
                Value::Text("+1".into()),
                Value::Text("-danger".into()),
                Value::Text("@SUM(A1)".into()),
                Value::Integer(-7),
            ]],
        );
        // Text cells get the apostrophe; the negative integer stays bare.
        assert_eq!(
            to_string(&r, ExportFormat::Csv),
            "a,b,c,d,e\n'=cmd|'/C calc'!A0,'+1,'-danger,'@SUM(A1),-7\n"
        );
    }

    #[test]
    fn csv_neutralizes_a_leading_tab() {
        // A leading tab is on OWASP's injection list but does not trigger
        // RFC-4180 quoting, so it must be apostrophe-prefixed too.
        let r = result(&["a"], vec![vec![Value::Text("\t=1+1".into())]]);
        assert_eq!(to_string(&r, ExportFormat::Csv), "a\n'\t=1+1\n");
    }

    #[test]
    fn json_keeps_formula_prefixes_verbatim() {
        let r = result(&["x"], vec![vec![Value::Text("=1+1".into())]]);
        assert_eq!(
            to_string(&r, ExportFormat::Json),
            "[\n  {\"x\":\"=1+1\"}\n]\n"
        );
    }

    #[test]
    fn csv_preserves_unicode_verbatim() {
        let r = result(&["s"], vec![vec![Value::Text("héllo · 世界".into())]]);
        assert_eq!(to_string(&r, ExportFormat::Csv), "s\nhéllo · 世界\n");
    }

    #[test]
    fn csv_empty_result_writes_only_the_header() {
        let r = result(&["a", "b"], vec![]);
        assert_eq!(to_string(&r, ExportFormat::Csv), "a,b\n");
    }

    #[test]
    fn json_encodes_every_type() {
        let r = result(
            &["n", "i", "r", "t", "b"],
            vec![vec![
                Value::Null,
                Value::Integer(42),
                Value::Real(2.5),
                Value::Text("hi".into()),
                Value::Blob(vec![0xde, 0xad]),
            ]],
        );
        assert_eq!(
            to_string(&r, ExportFormat::Json),
            "[\n  {\"n\":null,\"i\":42,\"r\":2.5,\"t\":\"hi\",\"b\":\"\\\\xdead\"}\n]\n"
        );
    }

    #[test]
    fn json_escapes_strings_and_keeps_unicode() {
        let r = result(&["say"], vec![vec![Value::Text("a\"b\\c\n\t世".into())]]);
        // Quotes/backslashes/control chars are escaped; the CJK char is kept
        // as UTF-8 (serde_json does not \u-escape it).
        assert_eq!(
            to_string(&r, ExportFormat::Json),
            "[\n  {\"say\":\"a\\\"b\\\\c\\n\\t世\"}\n]\n"
        );
    }

    #[test]
    fn json_multiple_rows_are_comma_separated() {
        let r = result(
            &["id"],
            vec![vec![Value::Integer(1)], vec![Value::Integer(2)]],
        );
        assert_eq!(
            to_string(&r, ExportFormat::Json),
            "[\n  {\"id\":1},\n  {\"id\":2}\n]\n"
        );
    }

    #[test]
    fn json_non_finite_real_degrades_to_a_string() {
        let r = result(&["x"], vec![vec![Value::Real(f64::NAN)]]);
        assert_eq!(
            to_string(&r, ExportFormat::Json),
            "[\n  {\"x\":\"NaN\"}\n]\n"
        );
    }

    #[test]
    fn json_empty_result_is_an_empty_array() {
        let r = result(&["a"], vec![]);
        assert_eq!(to_string(&r, ExportFormat::Json), "[]\n");
    }

    #[test]
    fn write_result_returns_the_row_count() {
        let r = result(
            &["id"],
            vec![
                vec![Value::Integer(1)],
                vec![Value::Integer(2)],
                vec![Value::Integer(3)],
            ],
        );
        let mut buf = Vec::new();
        assert_eq!(write_result(&r, ExportFormat::Csv, &mut buf).unwrap(), 3);
    }
}
