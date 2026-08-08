//! Clipboard rendering of a rectangular grid selection (FRE-110).
//!
//! The grid lifts the selected cells into a [`CopyBlock`] — column names plus
//! FULL values, never the grid's bounded previews — and [`render_copy`] turns
//! that into text for the clipboard in one of five encodings. Everything here
//! is pure: no signals, no I/O, no Dioxus.
//!
//! ## Which formats are faithful
//!
//! **JSON and INSERT reproduce a value exactly. TSV, CSV and Markdown do
//! not** — deliberately, and each in one specific way:
//!
//! - **TSV and CSV** neutralize spreadsheet formula injection (see
//!   [`harden_csv_text`], FRE-73): a text cell holding `-1` copies as `'-1`,
//!   and likewise for a leading `=`, `+`, `@` or tab. The destination decides
//!   this — these two formats exist to be pasted into a spreadsheet, and a
//!   spreadsheet executes such a cell **on paste**, not only on file open, so
//!   the threat is live on the clipboard path and not just the file-export
//!   one. Hardening here also keeps identical data behaving identically
//!   whether it leaves through Export or through Ctrl+C.
//!
//!   Note this is the *common* case, not an exotic one: **TSV is what the
//!   plain copy shortcut produces for a multi-cell selection**, so the
//!   default copy is a hardened copy.
//! - **Markdown** is a *reading* format for tickets and PR comments: it
//!   escapes pipes, folds newlines into `<br>`, and cannot tell an actual
//!   NULL from a text value that reads `NULL`.
//!
//! Nothing is lost from the feature, because there is always a faithful
//! option: **reach for JSON or INSERT when the point is to move data
//! unchanged.** Both leave every byte alone, and both are tested against live
//! engines for exactly that.
//!
//! ## NULL is never the empty string
//!
//! Within those limits, the one distinction every format does keep is the
//! classic silent-corruption bug here — a NULL that pastes back as `''`:
//!
//! | format   | `NULL`            | `Text("")` |
//! |----------|-------------------|------------|
//! | TSV/CSV  | empty, *unquoted* | `""`       |
//! | JSON     | `null`            | `""`       |
//! | INSERT   | `NULL`            | `''`       |
//! | Markdown | `NULL`            | empty cell |
//!
//! The CSV row is the one place this deliberately diverges from the file
//! export ([`super::export`]), which renders both as an empty field and
//! documents that as an accepted CSV limitation. The encoding used here is
//! not an invention: it is Postgres `COPY … WITH (FORMAT csv)`'s own
//! convention, where an unquoted empty field is NULL and `""` is the empty
//! string — verified by loading these exact bytes back through `COPY`. A file
//! export is read by a tool that brings its own NULL convention; a clipboard
//! copy usually lands somewhere that already agrees with this one. The
//! quoting rule itself is identical (RFC 4180), and a test below pins the two
//! renderings together for values where their semantics do agree.
//!
//! ## INSERT statements
//!
//! One statement per row (not a multi-row `VALUES` list, which SQL Server
//! caps at 1000 rows), naming exactly the selected columns and targeting the
//! source table, quoted with [`quote_ident`]. The literals are rendered for
//! **the dialect of the connection the rows came from** — see
//! [`sql_literal`] for the per-dialect text/blob/float rules, which is where
//! this format is easiest to get subtly wrong.

use std::fmt::Write as _;

use super::export::{harden_csv_text, hex_literal, write_result, ExportFormat};
use super::page::{quote_ident, Dialect};
use super::value::{ColumnInfo, QueryResult, Value};

/// The clipboard encodings offered by the grid's copy-as menu.
///
/// Each variant records whether it is faithful; see the module docs for why
/// the two spreadsheet formats are not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CopyFormat {
    /// Tab-separated — the universal spreadsheet paste target, and what the
    /// plain copy shortcut produces for a multi-cell selection. The header
    /// row is opt-in here (it is on by default for CSV and Markdown, whose
    /// consumers expect one).
    ///
    /// **Not faithful:** formula-hardened, like CSV. Being the default copy,
    /// this is the hardening most users will meet.
    Tsv { header: bool },
    /// RFC 4180 comma-separated, with a header row.
    ///
    /// **Not faithful:** formula-hardened, like TSV.
    Csv,
    /// An array of objects keyed by column name.
    ///
    /// **Faithful** — every value verbatim.
    Json,
    /// One `INSERT` statement per row, in the source connection's dialect.
    ///
    /// **Faithful** — every value verbatim, escaped for the dialect.
    Insert,
    /// A GitHub-flavoured Markdown table, with a header row.
    ///
    /// **Not faithful:** pipes are escaped, newlines fold to `<br>`, and a
    /// text value reading `NULL` is indistinguishable from a real one.
    Markdown,
}

impl CopyFormat {
    /// Menu label / status-line name for this format.
    pub fn label(self) -> &'static str {
        match self {
            CopyFormat::Tsv { header: false } => "TSV",
            CopyFormat::Tsv { header: true } => "TSV with header",
            CopyFormat::Csv => "CSV",
            CopyFormat::Json => "JSON",
            CopyFormat::Insert => "INSERT statements",
            CopyFormat::Markdown => "Markdown table",
        }
    }
}

/// A rectangular block of cells lifted out of the grid, ready to render.
///
/// `columns` names exactly the selected columns (a partial column selection
/// produces an INSERT/JSON over just those), and every row holds one value
/// per column in the same order. Values must be the **full** cell values: the
/// grid fetches anything it holds only a preview of before building this.
#[derive(Debug, Clone, PartialEq)]
pub struct CopyBlock {
    /// Schema of the source table, for the INSERT target.
    pub schema: Option<String>,
    /// Source table name, for the INSERT target.
    pub table: String,
    pub columns: Vec<String>,
    pub rows: Vec<Vec<Value>>,
}

/// Renders a selection for the clipboard. An empty block (no columns or no
/// rows) renders as an empty string, so copying nothing puts nothing on the
/// clipboard rather than a stray header.
///
/// Returns `None` for exactly one situation: [`CopyFormat::Insert`] without a
/// `dialect`. Every other format is dialect-neutral and ignores it. INSERT
/// refuses rather than falling back to a default flavour — a guessed dialect
/// produces SQL that parses and runs, just against the wrong engine's rules,
/// which is precisely the silent wrongness this format is guarded against.
pub fn render_copy(
    block: &CopyBlock,
    format: CopyFormat,
    dialect: Option<Dialect>,
) -> Option<String> {
    if format == CopyFormat::Insert && dialect.is_none() {
        return None;
    }
    if block.columns.is_empty() || block.rows.is_empty() {
        return Some(String::new());
    }
    Some(match format {
        CopyFormat::Tsv { header } => render_delimited(block, '\t', header),
        CopyFormat::Csv => render_delimited(block, ',', true),
        CopyFormat::Json => render_json(block),
        CopyFormat::Insert => render_insert(block, dialect?),
        CopyFormat::Markdown => render_markdown(block),
    })
}

/// The text a *single-cell* copy puts on the clipboard: the raw value alone,
/// with no delimiters, quoting, or header (FRE-110 keeps the pre-existing
/// single-cell behaviour). NULL copies as nothing — the point of this path is
/// "give me this value to paste", and pasting the word `NULL` into a form
/// field would be wrong; the delimited formats above are where NULL has to
/// stay distinguishable. Blobs copy as the same `\x…` hex the other formats
/// use rather than the grid's `<blob 2.0 KB>` placeholder.
///
/// **Faithful**, and deliberately *not* formula-hardened even though the
/// plain shortcut routes here: this path produces one bare value with no
/// delimiters, so it is not a spreadsheet document and pasting it into one
/// lands in a single cell the user is looking at. Copying that same cell as
/// TSV — a document — does harden it.
pub fn raw_cell_text(value: &Value) -> String {
    match value {
        Value::Null => String::new(),
        Value::Integer(i) => i.to_string(),
        Value::Real(r) => r.to_string(),
        Value::Text(t) => t.clone(),
        Value::Blob(b) => hex_literal(b),
    }
}

/// TSV/CSV: one record per line (LF), fields quoted per RFC 4180.
fn render_delimited(block: &CopyBlock, delimiter: char, header: bool) -> String {
    let mut out = String::new();
    if header {
        for (index, name) in block.columns.iter().enumerate() {
            if index > 0 {
                out.push(delimiter);
            }
            // A header is never NULL, but it can be empty or contain the
            // delimiter, so it goes through the same quoting.
            push_delimited_field(&mut out, Some(name), delimiter);
        }
        out.push('\n');
    }
    for row in &block.rows {
        for (index, value) in row.iter().enumerate() {
            if index > 0 {
                out.push(delimiter);
            }
            let cell = delimited_cell(value);
            push_delimited_field(&mut out, cell.as_deref(), delimiter);
        }
        out.push('\n');
    }
    out
}

/// The bare text of one cell for TSV/CSV, or `None` for NULL (which renders
/// as an unquoted empty field — see the module docs). Every other arm matches
/// [`super::export`]'s CSV cell rendering exactly, reusing its helpers:
/// spreadsheet formula prefixes are neutralized (FRE-73 — a clipboard paste
/// into Excel executes them just like an opened file does) and blobs render
/// as `\x…` hex.
fn delimited_cell(value: &Value) -> Option<String> {
    match value {
        Value::Null => None,
        Value::Integer(i) => Some(i.to_string()),
        Value::Real(r) => Some(r.to_string()),
        Value::Text(t) => Some(harden_csv_text(t)),
        Value::Blob(b) => Some(hex_literal(b)),
    }
}

/// Appends one delimited field, wrapping it in double quotes (and doubling
/// embedded quotes) when it contains the delimiter, a quote, or a line break
/// — plus, unlike the file export, when it is the **empty string**, so an
/// empty value reads as `""` and only a NULL is a bare empty field.
///
/// Deliberately not [`super::export`]'s `write_csv_field`: that one streams
/// into a `Write` for million-row exports (no per-field allocation) and hard-
/// codes the comma. The rule is the same; the two are pinned together by
/// `clipboard_csv_matches_the_export_for_shared_semantics` below.
fn push_delimited_field(out: &mut String, field: Option<&str>, delimiter: char) {
    let Some(field) = field else {
        return; // NULL: an empty, unquoted field.
    };
    if !field.is_empty() && !field.contains([delimiter, '"', '\n', '\r']) {
        out.push_str(field);
        return;
    }
    out.push('"');
    for ch in field.chars() {
        if ch == '"' {
            out.push('"');
        }
        out.push(ch);
    }
    out.push('"');
}

/// JSON: an array of objects keyed by column name. Delegates to the export
/// writer — its encoding (NULL → `null`, non-finite reals → their string
/// form, blobs → `\x…` hex, column order preserved) is exactly what is wanted
/// here, so there is one JSON renderer in the codebase, not two.
fn render_json(block: &CopyBlock) -> String {
    let result = QueryResult {
        columns: block
            .columns
            .iter()
            .map(|name| ColumnInfo { name: name.clone() })
            .collect(),
        rows: block.rows.clone(),
    };
    let mut buf = Vec::new();
    // Writing to a `Vec` cannot fail, and every byte serde_json emits is
    // valid UTF-8.
    let _ = write_result(&result, ExportFormat::Json, &mut buf);
    String::from_utf8(buf).unwrap_or_default()
}

/// One `INSERT … VALUES (…);` per row, over the selected columns.
fn render_insert(block: &CopyBlock, dialect: Dialect) -> String {
    let target = match &block.schema {
        Some(schema) => format!("{}.{}", quote_ident(schema), quote_ident(&block.table)),
        None => quote_ident(&block.table),
    };
    let names: Vec<String> = block.columns.iter().map(|c| quote_ident(c)).collect();
    let names = names.join(", ");
    let mut out = String::new();
    for row in &block.rows {
        let values: Vec<String> = row.iter().map(|v| sql_literal(v, dialect)).collect();
        let _ = writeln!(
            out,
            "INSERT INTO {target} ({names}) VALUES ({});",
            values.join(", ")
        );
    }
    out
}

/// One cell as an inline SQL literal for `dialect`. Inline rather than bound
/// (the staged-write path in [`super::staged`] binds parameters instead)
/// because the whole point of this format is text you can paste into another
/// client — which means every escaping rule has to be right here:
///
/// - **NULL** is the bare keyword, never `''`.
/// - **Text** doubles embedded single quotes. Backslashes are literal: on
///   Postgres that assumes `standard_conforming_strings`, on since 9.1 and
///   the only setting we support. On SQL Server the literal is `N'…'` —
///   an unprefixed T-SQL literal is `varchar`, which replaces every character
///   outside the database's code page with `?`, so a plain `'…'` would
///   silently mangle non-Latin-1 text.
/// - **Blobs** use each dialect's binary literal: `X'ab'` (SQLite),
///   `'\xab'::bytea` (Postgres), `0xab` (SQL Server). An empty blob is
///   `X''` / `'\x'::bytea` / `0x`, all valid.
/// - **Finite reals** use Rust's `Debug` form, not `Display`: both round-trip
///   exactly, but `Display` never uses an exponent, so `1e300` spells itself
///   out as 301 digits — which SQL Server rejects outright ("the number … is
///   too long. Maximum length is 128"). `Debug` gives `1e300`, and keeps the
///   decimal point on whole values (`2.0`, not `2`) so a real stays a real.
/// - **Non-finite reals** have no numeric literal form. Postgres spells them
///   as quoted floats (`'NaN'::double precision`). SQLite has no NaN at all —
///   it stores one as NULL, which is what a NaN pasted back into SQLite would
///   become anyway — but `9e999` does overflow to infinity there. T-SQL
///   `float` cannot hold either, so such a value cannot have come from SQL
///   Server and renders as NULL.
pub(crate) fn sql_literal(value: &Value, dialect: Dialect) -> String {
    match value {
        Value::Null => "NULL".to_string(),
        Value::Integer(i) => i.to_string(),
        Value::Real(r) => real_literal(*r, dialect),
        Value::Text(t) => text_literal(t, dialect),
        Value::Blob(b) => blob_literal(b, dialect),
    }
}

fn text_literal(text: &str, dialect: Dialect) -> String {
    let escaped = text.replace('\'', "''");
    match dialect {
        Dialect::SqlServer => format!("N'{escaped}'"),
        Dialect::Sqlite | Dialect::Postgres => format!("'{escaped}'"),
    }
}

fn blob_literal(bytes: &[u8], dialect: Dialect) -> String {
    let mut hex = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(hex, "{byte:02x}");
    }
    match dialect {
        Dialect::Sqlite => format!("X'{hex}'"),
        Dialect::Postgres => format!("'\\x{hex}'::bytea"),
        Dialect::SqlServer => format!("0x{hex}"),
    }
}

fn real_literal(real: f64, dialect: Dialect) -> String {
    if real.is_finite() {
        // `{:?}`, not `{}` — see the note on `sql_literal`. The delimited and
        // JSON formats deliberately keep `Display` (matching the file export);
        // only SQL needs the bounded exponent form.
        return format!("{real:?}");
    }
    match (dialect, real.is_nan(), real.is_sign_positive()) {
        (Dialect::Postgres, true, _) => "'NaN'::double precision".to_string(),
        (Dialect::Postgres, false, true) => "'Infinity'::double precision".to_string(),
        (Dialect::Postgres, false, false) => "'-Infinity'::double precision".to_string(),
        (Dialect::Sqlite, false, true) => "9e999".to_string(),
        (Dialect::Sqlite, false, false) => "-9e999".to_string(),
        // SQLite NaN, and every non-finite on SQL Server.
        _ => "NULL".to_string(),
    }
}

/// A GitHub-flavoured Markdown table with a header row.
fn render_markdown(block: &CopyBlock) -> String {
    let mut out = String::new();
    out.push_str("| ");
    out.push_str(
        &block
            .columns
            .iter()
            .map(|name| markdown_cell_text(name))
            .collect::<Vec<_>>()
            .join(" | "),
    );
    out.push_str(" |\n|");
    for _ in &block.columns {
        out.push_str(" --- |");
    }
    out.push('\n');
    for row in &block.rows {
        out.push_str("| ");
        out.push_str(
            &row.iter()
                .map(markdown_value)
                .collect::<Vec<_>>()
                .join(" | "),
        );
        out.push_str(" |\n");
    }
    out
}

/// One value as Markdown table-cell text. NULL is spelled out (an empty cell
/// means the empty string); everything else renders like the other formats,
/// then gets its table-breaking characters neutralized.
fn markdown_value(value: &Value) -> String {
    match value {
        Value::Null => "NULL".to_string(),
        Value::Integer(i) => i.to_string(),
        Value::Real(r) => r.to_string(),
        Value::Text(t) => markdown_cell_text(t),
        Value::Blob(b) => hex_literal(b),
    }
}

/// Escapes the two things that break a Markdown table row: a `|` (which would
/// start a new cell) and a line break (which would end the row). No other
/// Markdown escaping — inline emphasis inside a cell is cosmetic, and over-
/// escaping would make the common case unreadable in a ticket.
fn markdown_cell_text(text: &str) -> String {
    text.replace('\r', "")
        .replace('|', "\\|")
        .replace('\n', "<br>")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn block(columns: &[&str], rows: Vec<Vec<Value>>) -> CopyBlock {
        CopyBlock {
            schema: None,
            table: "t".into(),
            columns: columns.iter().map(|c| (*c).to_string()).collect(),
            rows,
        }
    }

    /// Renders `format`, asserting it is one of the cases that always
    /// succeeds (see [`render_copy`]: only INSERT-without-a-dialect is
    /// `None`).
    fn render(block: &CopyBlock, format: CopyFormat, dialect: Option<Dialect>) -> String {
        render_copy(block, format, dialect).expect("this format always renders")
    }

    fn tsv(block: &CopyBlock) -> String {
        render(block, CopyFormat::Tsv { header: false }, None)
    }

    fn csv(block: &CopyBlock) -> String {
        render(block, CopyFormat::Csv, None)
    }

    fn json(block: &CopyBlock) -> String {
        render(block, CopyFormat::Json, None)
    }

    fn markdown(block: &CopyBlock) -> String {
        render(block, CopyFormat::Markdown, None)
    }

    fn insert(block: &CopyBlock, dialect: Dialect) -> String {
        render(block, CopyFormat::Insert, Some(dialect))
    }

    // ---- empty / single cell -------------------------------------------

    #[test]
    fn an_empty_selection_renders_as_nothing_in_every_format() {
        let no_rows = block(&["a"], vec![]);
        let no_columns = block(&[], vec![vec![]]);
        for format in [
            CopyFormat::Tsv { header: true },
            CopyFormat::Csv,
            CopyFormat::Json,
            CopyFormat::Insert,
            CopyFormat::Markdown,
        ] {
            assert_eq!(render(&no_rows, format, Some(Dialect::Sqlite)), "");
            assert_eq!(render(&no_columns, format, Some(Dialect::Sqlite)), "");
        }
    }

    #[test]
    fn a_single_cell_still_renders_a_full_document_per_format() {
        let b = block(&["a"], vec![vec![Value::Integer(7)]]);
        assert_eq!(tsv(&b), "7\n");
        assert_eq!(csv(&b), "a\n7\n");
        assert_eq!(json(&b), "[\n  {\"a\":7}\n]\n");
        assert_eq!(
            insert(&b, Dialect::Sqlite),
            "INSERT INTO \"t\" (\"a\") VALUES (7);\n"
        );
        assert_eq!(markdown(&b), "| a |\n| --- |\n| 7 |\n");
    }

    #[test]
    fn raw_cell_text_copies_the_bare_value() {
        assert_eq!(raw_cell_text(&Value::Text("hi\tthere".into())), "hi\tthere");
        assert_eq!(raw_cell_text(&Value::Integer(-3)), "-3");
        assert_eq!(raw_cell_text(&Value::Real(1.5)), "1.5");
        // NULL copies as nothing; a blob copies as hex, not "<blob 2 B>".
        assert_eq!(raw_cell_text(&Value::Null), "");
        assert_eq!(raw_cell_text(&Value::Blob(vec![0xde, 0xad])), "\\xdead");
        // The empty string is also nothing — this path does not distinguish
        // them; the delimited formats do.
        assert_eq!(raw_cell_text(&Value::Text(String::new())), "");
    }

    // ---- NULL vs the empty string --------------------------------------

    #[test]
    fn null_is_distinguishable_from_the_empty_string_in_every_format() {
        let b = block(
            &["n", "e"],
            vec![vec![Value::Null, Value::Text(String::new())]],
        );
        // Delimited: a bare field for NULL, `""` for the empty string.
        assert_eq!(tsv(&b), "\t\"\"\n");
        assert_eq!(csv(&b), "n,e\n,\"\"\n");
        assert_eq!(json(&b), "[\n  {\"n\":null,\"e\":\"\"}\n]\n");
        assert_eq!(
            insert(&b, Dialect::Sqlite),
            "INSERT INTO \"t\" (\"n\", \"e\") VALUES (NULL, '');\n"
        );
        assert_eq!(markdown(&b), "| n | e |\n| --- | --- |\n| NULL |  |\n");
    }

    // ---- delimited formats ---------------------------------------------

    #[test]
    fn tsv_omits_the_header_unless_asked() {
        let b = block(
            &["id", "title"],
            vec![
                vec![Value::Integer(1), Value::Text("one".into())],
                vec![Value::Integer(2), Value::Text("two".into())],
            ],
        );
        assert_eq!(tsv(&b), "1\tone\n2\ttwo\n");
        assert_eq!(
            render(&b, CopyFormat::Tsv { header: true }, None),
            "id\ttitle\n1\tone\n2\ttwo\n"
        );
    }

    #[test]
    fn tsv_quotes_tabs_quotes_and_newlines_but_not_commas() {
        let b = block(
            &["a", "b", "c", "d"],
            vec![vec![
                Value::Text("has\ttab".into()),
                Value::Text("has\"quote".into()),
                Value::Text("two\nlines".into()),
                Value::Text("a,b".into()),
            ]],
        );
        // The comma is not the delimiter here, so it stays bare.
        assert_eq!(
            tsv(&b),
            "\"has\ttab\"\t\"has\"\"quote\"\t\"two\nlines\"\ta,b\n"
        );
    }

    #[test]
    fn csv_quotes_commas_quotes_and_newlines_but_not_tabs() {
        let b = block(
            &["a", "b", "c", "d"],
            vec![vec![
                Value::Text("a,b".into()),
                Value::Text("has\"quote".into()),
                Value::Text("two\nlines".into()),
                Value::Text("has\ttab".into()),
            ]],
        );
        assert_eq!(
            csv(&b),
            "a,b,c,d\n\"a,b\",\"has\"\"quote\",\"two\nlines\",has\ttab\n"
        );
    }

    #[test]
    fn delimited_formats_harden_formula_prefixes_and_hex_encode_blobs() {
        let b = block(
            &["f", "b"],
            vec![vec![
                Value::Text("=1+1".into()),
                Value::Blob(vec![0x00, 0xff]),
            ]],
        );
        // A clipboard paste into Excel runs formulas exactly like an opened
        // file does, so FRE-73's apostrophe applies here too.
        assert_eq!(tsv(&b), "'=1+1\t\\x00ff\n");
    }

    #[test]
    fn only_the_spreadsheet_formats_harden_formula_prefixes() {
        // The flip side of the hardening: TSV/CSV are lossy for a value
        // starting with `=` `+` `-` `@` or a tab, and JSON/INSERT — the two
        // formats documented as exact — must leave it completely alone.
        let b = block(&["f"], vec![vec![Value::Text("-1".into())]]);
        assert_eq!(tsv(&b), "'-1\n");
        assert_eq!(csv(&b), "f\n'-1\n");
        assert_eq!(json(&b), "[\n  {\"f\":\"-1\"}\n]\n");
        assert_eq!(
            insert(&b, Dialect::Sqlite),
            "INSERT INTO \"t\" (\"f\") VALUES ('-1');\n"
        );
        // Markdown is lossy in other ways, but not this one.
        assert_eq!(markdown(&b), "| f |\n| --- |\n| -1 |\n");
    }

    #[test]
    fn a_header_containing_the_delimiter_is_quoted_too() {
        let b = block(&["odd,name"], vec![vec![Value::Integer(1)]]);
        assert_eq!(csv(&b), "\"odd,name\"\n1\n");
    }

    #[test]
    fn clipboard_csv_matches_the_export_for_shared_semantics() {
        // Anti-drift: for values where the two agree (no NULLs, no empty
        // strings), the clipboard's CSV must be byte-identical to the file
        // export's — same quoting, same escaping, same hardening.
        let rows = vec![
            vec![
                Value::Text("plain".into()),
                Value::Text("has,comma".into()),
                Value::Text("has\"quote".into()),
            ],
            vec![
                Value::Text("line1\nline2\r".into()),
                Value::Integer(-7),
                Value::Blob(vec![0x10, 0x20]),
            ],
            vec![
                Value::Text("=cmd".into()),
                Value::Real(1.5),
                Value::Text("héllo · 世界".into()),
            ],
        ];
        let b = block(&["a", "b", "c"], rows.clone());
        let result = QueryResult {
            columns: b
                .columns
                .iter()
                .map(|name| ColumnInfo { name: name.clone() })
                .collect(),
            rows,
        };
        let mut exported = Vec::new();
        write_result(&result, ExportFormat::Csv, &mut exported).unwrap();
        assert_eq!(csv(&b), String::from_utf8(exported).unwrap());
    }

    #[test]
    fn unicode_survives_every_format_verbatim() {
        let b = block(&["s"], vec![vec![Value::Text("héllo · 世界 🦀".into())]]);
        assert_eq!(tsv(&b), "héllo · 世界 🦀\n");
        assert_eq!(csv(&b), "s\nhéllo · 世界 🦀\n");
        assert_eq!(json(&b), "[\n  {\"s\":\"héllo · 世界 🦀\"}\n]\n");
        assert_eq!(
            insert(&b, Dialect::Postgres),
            "INSERT INTO \"t\" (\"s\") VALUES ('héllo · 世界 🦀');\n"
        );
        assert_eq!(markdown(&b), "| s |\n| --- |\n| héllo · 世界 🦀 |\n");
    }

    // ---- JSON ------------------------------------------------------------

    #[test]
    fn json_is_an_array_of_objects_keyed_by_column() {
        let b = block(
            &["id", "title"],
            vec![
                vec![Value::Integer(1), Value::Text("one".into())],
                vec![Value::Integer(2), Value::Null],
            ],
        );
        assert_eq!(
            json(&b),
            "[\n  {\"id\":1,\"title\":\"one\"},\n  {\"id\":2,\"title\":null}\n]\n"
        );
    }

    #[test]
    fn json_escapes_control_characters_and_quotes() {
        let b = block(&["s"], vec![vec![Value::Text("a\"b\\c\n\t".into())]]);
        assert_eq!(json(&b), "[\n  {\"s\":\"a\\\"b\\\\c\\n\\t\"}\n]\n");
    }

    // ---- Markdown --------------------------------------------------------

    #[test]
    fn markdown_escapes_pipes_and_folds_newlines() {
        let b = block(
            &["a|b"],
            vec![
                vec![Value::Text("x|y".into())],
                vec![Value::Text("two\r\nlines".into())],
            ],
        );
        assert_eq!(
            markdown(&b),
            "| a\\|b |\n| --- |\n| x\\|y |\n| two<br>lines |\n"
        );
    }

    #[test]
    fn markdown_renders_every_type() {
        let b = block(
            &["n", "i", "r", "t", "b"],
            vec![vec![
                Value::Null,
                Value::Integer(-1),
                Value::Real(2.5),
                Value::Text("hi".into()),
                Value::Blob(vec![0xab]),
            ]],
        );
        assert_eq!(
            markdown(&b),
            "| n | i | r | t | b |\n\
             | --- | --- | --- | --- | --- |\n\
             | NULL | -1 | 2.5 | hi | \\xab |\n"
        );
    }

    // ---- INSERT, per dialect ---------------------------------------------

    fn wide_block() -> CopyBlock {
        CopyBlock {
            schema: Some("app data".into()),
            table: "tra\"cks".into(),
            columns: vec!["id".into(), "na\"me".into(), "note".into(), "raw".into()],
            rows: vec![
                vec![
                    Value::Integer(1),
                    Value::Text("O'Brien".into()),
                    Value::Null,
                    Value::Blob(vec![0xde, 0xad, 0xbe, 0xef]),
                ],
                vec![
                    Value::Integer(2),
                    Value::Text("back\\slash".into()),
                    Value::Text(String::new()),
                    Value::Blob(vec![]),
                ],
            ],
        }
    }

    #[test]
    fn insert_sqlite_quotes_idents_escapes_quotes_and_uses_x_blobs() {
        assert_eq!(
            insert(&wide_block(), Dialect::Sqlite),
            "INSERT INTO \"app data\".\"tra\"\"cks\" (\"id\", \"na\"\"me\", \"note\", \"raw\") \
             VALUES (1, 'O''Brien', NULL, X'deadbeef');\n\
             INSERT INTO \"app data\".\"tra\"\"cks\" (\"id\", \"na\"\"me\", \"note\", \"raw\") \
             VALUES (2, 'back\\slash', '', X'');\n"
        );
    }

    #[test]
    fn insert_postgres_uses_bytea_hex_literals() {
        assert_eq!(
            insert(&wide_block(), Dialect::Postgres),
            "INSERT INTO \"app data\".\"tra\"\"cks\" (\"id\", \"na\"\"me\", \"note\", \"raw\") \
             VALUES (1, 'O''Brien', NULL, '\\xdeadbeef'::bytea);\n\
             INSERT INTO \"app data\".\"tra\"\"cks\" (\"id\", \"na\"\"me\", \"note\", \"raw\") \
             VALUES (2, 'back\\slash', '', '\\x'::bytea);\n"
        );
    }

    #[test]
    fn insert_sqlserver_uses_n_prefixed_text_and_0x_blobs() {
        // The N prefix is the whole point: without it T-SQL parses the
        // literal as varchar and drops anything outside the code page.
        assert_eq!(
            insert(&wide_block(), Dialect::SqlServer),
            "INSERT INTO \"app data\".\"tra\"\"cks\" (\"id\", \"na\"\"me\", \"note\", \"raw\") \
             VALUES (1, N'O''Brien', NULL, 0xdeadbeef);\n\
             INSERT INTO \"app data\".\"tra\"\"cks\" (\"id\", \"na\"\"me\", \"note\", \"raw\") \
             VALUES (2, N'back\\slash', N'', 0x);\n"
        );
    }

    #[test]
    fn insert_omits_the_schema_when_the_table_has_none() {
        let b = block(&["a"], vec![vec![Value::Text("x".into())]]);
        assert_eq!(
            insert(&b, Dialect::Postgres),
            "INSERT INTO \"t\" (\"a\") VALUES ('x');\n"
        );
    }

    #[test]
    fn insert_covers_only_the_selected_columns() {
        // A two-column selection out of a five-column table names two.
        let b = block(
            &["title", "year"],
            vec![vec![
                Value::Text("Kind of Blue".into()),
                Value::Integer(1959),
            ]],
        );
        assert_eq!(
            insert(&b, Dialect::Sqlite),
            "INSERT INTO \"t\" (\"title\", \"year\") VALUES ('Kind of Blue', 1959);\n"
        );
    }

    #[test]
    fn insert_keeps_newlines_and_unicode_inside_text_literals() {
        let b = block(&["s"], vec![vec![Value::Text("two\nlines ⏎".into())]]);
        // A literal newline inside '…' is legal SQL everywhere; the statement
        // just spans two lines.
        assert_eq!(
            insert(&b, Dialect::Sqlite),
            "INSERT INTO \"t\" (\"s\") VALUES ('two\nlines ⏎');\n"
        );
    }

    #[test]
    fn insert_renders_non_finite_reals_per_dialect() {
        let b = block(
            &["nan", "inf", "ninf"],
            vec![vec![
                Value::Real(f64::NAN),
                Value::Real(f64::INFINITY),
                Value::Real(f64::NEG_INFINITY),
            ]],
        );
        assert_eq!(
            insert(&b, Dialect::Postgres),
            "INSERT INTO \"t\" (\"nan\", \"inf\", \"ninf\") VALUES \
             ('NaN'::double precision, 'Infinity'::double precision, '-Infinity'::double precision);\n"
        );
        // SQLite: 9e999 overflows to infinity; a NaN becomes NULL, which is
        // exactly what SQLite itself stores for one.
        assert_eq!(
            insert(&b, Dialect::Sqlite),
            "INSERT INTO \"t\" (\"nan\", \"inf\", \"ninf\") VALUES (NULL, 9e999, -9e999);\n"
        );
        // T-SQL float holds neither, so such a value cannot have come from
        // SQL Server in the first place.
        assert_eq!(
            insert(&b, Dialect::SqlServer),
            "INSERT INTO \"t\" (\"nan\", \"inf\", \"ninf\") VALUES (NULL, NULL, NULL);\n"
        );
    }

    #[test]
    fn insert_renders_finite_reals_in_a_bounded_exponent_form() {
        let b = block(
            &["r", "whole", "huge", "tiny", "big"],
            vec![vec![
                Value::Real(-0.125),
                Value::Real(2.0),
                Value::Real(1e300),
                Value::Real(1.5e-8),
                Value::Integer(i64::MIN),
            ]],
        );
        // `1e300` must stay `1e300`: Rust's Display would write 301 digits,
        // and SQL Server rejects a numeric literal over 128 characters. A
        // whole value keeps its `.0` so it stays a float, not an integer.
        assert_eq!(
            insert(&b, Dialect::SqlServer),
            "INSERT INTO \"t\" (\"r\", \"whole\", \"huge\", \"tiny\", \"big\") \
             VALUES (-0.125, 2.0, 1e300, 1.5e-8, -9223372036854775808);\n"
        );
        // The delimited/JSON formats keep the plain Display form, matching the
        // file export — only SQL literals need the exponent.
        assert!(!json(&b).contains("1e300"));
    }

    #[test]
    fn sql_literal_never_leaves_a_quote_unescaped() {
        // Property-ish sanity check on the escaping: whatever the input, the
        // rendered literal has an even number of single quotes, so it cannot
        // terminate early and turn the rest of the row into SQL.
        for text in [
            "'",
            "''",
            "a'b'c",
            "'; DROP TABLE t; --",
            "\\'",
            "ends with '",
        ] {
            for dialect in [Dialect::Sqlite, Dialect::Postgres, Dialect::SqlServer] {
                let literal = sql_literal(&Value::Text(text.into()), dialect);
                assert_eq!(
                    literal.matches('\'').count() % 2,
                    0,
                    "unbalanced quotes in {literal}"
                );
                assert!(literal.ends_with('\''), "literal not closed: {literal}");
            }
        }
    }

    #[test]
    fn insert_without_a_dialect_refuses_instead_of_guessing_one() {
        let b = block(&["a"], vec![vec![Value::Text("x".into())]]);
        assert_eq!(render_copy(&b, CopyFormat::Insert, None), None);
        // Every other format is dialect-neutral and renders regardless.
        for format in [
            CopyFormat::Tsv { header: false },
            CopyFormat::Tsv { header: true },
            CopyFormat::Csv,
            CopyFormat::Json,
            CopyFormat::Markdown,
        ] {
            assert!(
                render_copy(&b, format, None).is_some(),
                "{} needs no dialect",
                format.label()
            );
        }
        // An empty selection still refuses INSERT without a dialect, so the
        // `None` return never has to be disambiguated from "nothing to copy".
        let empty = block(&["a"], vec![]);
        assert_eq!(render_copy(&empty, CopyFormat::Insert, None), None);
        assert_eq!(
            render_copy(&empty, CopyFormat::Insert, Some(Dialect::Sqlite)),
            Some(String::new())
        );
    }

    #[test]
    fn format_labels_are_distinct() {
        let labels = [
            CopyFormat::Tsv { header: false }.label(),
            CopyFormat::Tsv { header: true }.label(),
            CopyFormat::Csv.label(),
            CopyFormat::Json.label(),
            CopyFormat::Insert.label(),
            CopyFormat::Markdown.label(),
        ];
        for (i, a) in labels.iter().enumerate() {
            for b in &labels[i + 1..] {
                assert_ne!(a, b);
            }
        }
    }
}
