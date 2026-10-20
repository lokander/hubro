//! Importing a CSV or JSON file into an **existing** table — the
//! inverse of [`export`](super::export), and built the same way: records are
//! pulled from the file ONE AT A TIME and written in batches, so peak memory
//! is one batch regardless of file size.
//!
//! # What it will not do
//!
//! Create a table. The target's [`ColumnMeta`] is what supplies the types, so
//! there is nothing to infer and nothing to guess. Inferring a schema from a
//! file is deliberately not import's job.
//!
//! # Abort by default, skip on request
//!
//! A bad row **aborts the import and rolls the whole thing back**, unless the
//! user chose [`ErrorMode::Skip`] before starting. A half-imported table
//! nobody asked for is worse than a failed import: it is silent, and it is
//! discovered later by whoever trusts the data. Skipping is legitimate when
//! cleaning a messy export, but it is a decision made up front, and every
//! skipped row is reported with its line ([`ImportReport::skipped`]).
//!
//! The rollback is not a claim about the file — it is a transaction. Every
//! batch runs inside ONE [`ScriptTx`](super::registry::ScriptTx) and the
//! commit happens after the last record is read, so a failure anywhere undoes
//! every row already sent ([`ImportError::undone_rows`] says how many that
//! was).
//!
//! **Skip mode skips the rows hubro itself rejected**, during coercion, before
//! any SQL was built for them. A row the *server* rejects aborts the import in
//! both modes: on Postgres the failed statement has already doomed the
//! transaction, so "carry on" would mean committing nothing anyway, and an
//! engine-dependent answer to "what happened to my import" is worse than a
//! uniform one. Coercion is therefore where the checking happens — see
//! [`coerce_field`](super::coerce::coerce_field) for what it can and cannot
//! catch.
//!
//! # A NULL where the column cannot hold one
//!
//! An empty field (under [`EmptyField::Null`]), a JSON `null` and an absent
//! JSON key all coerce to NULL. Where the target column is NOT NULL that
//! NULL cannot be stored, and [`null_cell`] decides what happens instead: a
//! column the database fills in itself (a `DEFAULT`, a `serial`, an identity)
//! gets that value, and any other row is refused during coercion, where skip
//! mode can skip it. Sending the NULL anyway is never an option — Postgres,
//! SQL Server and SQLite all reject it, and a rejection by the server is the
//! one failure skip mode cannot skip.
//!
//! # No transaction, no import
//!
//! [`import_refusal`] refuses an import wherever editing is refused, and
//! additionally wherever the connection cannot hold a transaction — the same
//! reasoning as [`NO_GUARDED_WRITE`](super::caps::NO_GUARDED_WRITE), only
//! sharper: an import is the one write where a mid-way failure is *likely*
//! (files are messy), so running one without a way back is the worst place to
//! offer unguarded writes. That refusal is what "say so before starting"
//! amounts to here.

use std::fs::File;
use std::io::{self, BufReader};
use std::path::Path;

use super::caps::{self, TableAccess};
use super::coerce::coerce_field;
use super::error::DbError;
use super::registry::{DbPool, ScriptTx};
use super::schema::{ColumnMeta, Generated, TableMeta};
use super::sql::{qualified, quote_ident, Dialect};
use super::staged::{cast_targets, ParamSql};
use super::value::Value;

pub mod csv;
mod cursor;
pub mod json;

pub use csv::{sniff_dialect, CsvDialect, CsvReader, SNIFF_BYTES};
pub use json::{sniff_shape, JsonReader, JsonShape};

/// How many skipped rows are reported individually. The count is exact
/// however many there are ([`ImportReport::skipped_rows`]); only the
/// line-by-line list is capped, so a million-row file of garbage can't make
/// the report cost more memory than the import did.
pub const MAX_REPORTED_SKIPS: usize = 200;

/// Text encoding of the file being read. UTF-8 is the default and the right
/// answer for almost everything; Latin-1 exists because files exported from
/// older systems do, and they are not valid UTF-8 at all — without it such a
/// file fails at the first accented character instead of importing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Encoding {
    #[default]
    Utf8,
    /// ISO-8859-1: every byte is the code point of the same number, so the
    /// decode cannot fail. Also what a mislabelled Windows-1252 file decodes
    /// as, minus its 0x80–0x9F punctuation.
    Latin1,
}

impl Encoding {
    pub fn label(self) -> &'static str {
        match self {
            Encoding::Utf8 => "UTF-8",
            Encoding::Latin1 => "Latin-1 (ISO-8859-1)",
        }
    }
}

/// Decodes raw field bytes per `encoding`, reporting invalid UTF-8 rather
/// than replacing it: a silently mangled value is worse than a message
/// naming the line and pointing at the encoding option.
pub(crate) fn decode_bytes(bytes: &[u8], encoding: Encoding) -> Result<String, String> {
    match encoding {
        Encoding::Utf8 => String::from_utf8(bytes.to_vec())
            .map_err(|_| "not valid UTF-8 — try the Latin-1 encoding option".to_string()),
        Encoding::Latin1 => Ok(bytes.iter().map(|b| *b as char).collect()),
    }
}

/// Guesses the encoding: UTF-8 unless the sample proves it is not, which is
/// exactly the case Latin-1 exists for. A file whose non-ASCII bytes happen
/// to form valid UTF-8 sequences is UTF-8 as far as anyone can tell from the
/// bytes; the override is there for the rest.
pub fn sniff_encoding(sample: &[u8]) -> Encoding {
    match std::str::from_utf8(sample) {
        Ok(_) => Encoding::Utf8,
        // The sample is a fixed-size slice of the file, so it usually ends
        // mid-character. `error_len() == None` is exactly that case — valid
        // UTF-8 that simply has not finished — and is no evidence of
        // anything; a real Latin-1 file reports a definite error instead.
        Err(err) if err.error_len().is_none() => Encoding::Utf8,
        Err(_) => Encoding::Latin1,
    }
}

/// A failure while reading the file itself — malformed input or I/O, as
/// opposed to a value that will not fit its column.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadError {
    /// 1-based physical line the record started on, when one is known.
    pub line: Option<u64>,
    pub message: String,
}

/// Which field of a record a column is fed from: a position (CSV, whose
/// fields are ordered) or a key (JSON, whose fields are named).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SourceField {
    Index(usize),
    Key(String),
}

impl SourceField {
    /// How this field is named in the mapping UI and in error messages.
    pub fn label(&self, header: Option<&[String]>) -> String {
        match self {
            SourceField::Index(index) => match header.and_then(|names| names.get(*index)) {
                Some(name) => name.clone(),
                None => format!("Column {}", index + 1),
            },
            SourceField::Key(key) => key.clone(),
        }
    }
}

/// One field's value as the file gave it. CSV yields [`Self::Text`]; JSON
/// yields [`Self::Json`], which keeps null, numbers and booleans distinct
/// from their text spelling.
#[derive(Debug, Clone, PartialEq)]
pub enum SourceValue {
    Text(String),
    Json(serde_json::Value),
    /// The record had no such field at all.
    Missing,
}

impl SourceValue {
    /// Plain-text rendering for the preview grid.
    pub fn display(&self) -> String {
        match self {
            SourceValue::Text(text) => text.clone(),
            SourceValue::Json(serde_json::Value::String(text)) => text.clone(),
            SourceValue::Json(value) => value.to_string(),
            SourceValue::Missing => String::new(),
        }
    }
}

/// One record's fields, in whichever way its format addresses them.
#[derive(Debug, Clone, PartialEq)]
pub enum RecordFields {
    Positional(Vec<String>),
    Keyed(serde_json::Map<String, serde_json::Value>),
}

/// One record read from the file.
#[derive(Debug, Clone, PartialEq)]
pub struct Record {
    /// 1-based physical line the record starts on — what a skip report names.
    pub line: u64,
    pub fields: RecordFields,
}

impl Record {
    pub fn positional(line: u64, fields: Vec<String>) -> Record {
        Record {
            line,
            fields: RecordFields::Positional(fields),
        }
    }

    pub fn keyed(line: u64, fields: serde_json::Map<String, serde_json::Value>) -> Record {
        Record {
            line,
            fields: RecordFields::Keyed(fields),
        }
    }

    /// One field's value, or [`SourceValue::Missing`] when the record has no
    /// such field — a short CSV row or a JSON object without that key.
    pub fn field(&self, source: &SourceField) -> SourceValue {
        match (&self.fields, source) {
            (RecordFields::Positional(values), SourceField::Index(index)) => values
                .get(*index)
                .map(|text| SourceValue::Text(text.clone()))
                .unwrap_or(SourceValue::Missing),
            (RecordFields::Keyed(map), SourceField::Key(key)) => map
                .get(key)
                .map(|value| SourceValue::Json(value.clone()))
                .unwrap_or(SourceValue::Missing),
            // A mapping built for the other format addresses nothing.
            _ => SourceValue::Missing,
        }
    }

    /// The fields this record actually carries, in the order the file has
    /// them — JSON keys included, which is what serde_json's
    /// `preserve_order` feature is enabled for (without it a `Map` is a
    /// `BTreeMap`, and the preview listed every JSON file alphabetically).
    pub fn present_fields(&self) -> Vec<SourceField> {
        match &self.fields {
            RecordFields::Positional(values) => (0..values.len()).map(SourceField::Index).collect(),
            RecordFields::Keyed(map) => map.keys().cloned().map(SourceField::Key).collect(),
        }
    }
}

/// A pull-based stream of records. Implemented by [`CsvReader`] and
/// [`JsonReader`]; `run_import` only ever asks for the next one, which is
/// what keeps the import streaming.
///
/// Two shapes are accepted rather than refused, and both are visible in the
/// dialog's preview before anything is imported:
///
/// - a record with **more** fields than the header names: the surplus is not
///   mapped to anything, so it is dropped. (Usually the symptom of a wrong
///   delimiter — which the preview shows as one column too many.) A record
///   with **fewer** is equally fine: the missing fields are
///   [`SourceValue::Missing`], i.e. NULL.
/// - a JSON object with the same key twice: `serde_json` keeps the last, so
///   the last wins here too.
pub trait RecordSource {
    /// The file's own field names, when it has them (a CSV header). `None`
    /// means fields are addressed by position or by key instead.
    fn field_names(&mut self) -> Result<Option<Vec<String>>, ReadError>;

    /// The next record, or `None` at the end of the file.
    fn next_record(&mut self) -> Result<Option<Record>, ReadError>;
}

/// Which reader a file gets, and how it is configured.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceFormat {
    Csv(CsvDialect),
    Json(JsonShape),
}

impl SourceFormat {
    pub fn label(&self) -> &'static str {
        match self {
            SourceFormat::Csv(_) => "CSV",
            SourceFormat::Json(JsonShape::Array) => "JSON (array)",
            SourceFormat::Json(JsonShape::Lines) => "JSON (one per line)",
        }
    }
}

/// Opens `path` as a record source. The reader is buffered, so records are
/// parsed out of a fixed-size buffer rather than the file being read into
/// memory.
pub fn open_source(
    path: &Path,
    format: SourceFormat,
    encoding: Encoding,
) -> io::Result<Box<dyn RecordSource>> {
    let file = BufReader::new(File::open(path)?);
    Ok(match format {
        SourceFormat::Csv(dialect) => Box::new(CsvReader::new(file, dialect, encoding)),
        SourceFormat::Json(shape) => Box::new(JsonReader::new(file, shape, encoding)),
    })
}

/// What the head of a file suggests about how to read it — every field of it
/// overridable, because detection is evidence, not a verdict.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileSniff {
    pub format: SourceFormat,
    pub encoding: Encoding,
}

/// Reads the first [`SNIFF_BYTES`] of `path` and guesses its format, dialect
/// and encoding. The extension picks CSV vs JSON (it is the one thing the
/// user already told us); everything else comes from the bytes.
pub fn sniff_file(path: &Path) -> io::Result<FileSniff> {
    use std::io::Read as _;
    let mut head = Vec::new();
    File::open(path)?
        .take(SNIFF_BYTES as u64)
        .read_to_end(&mut head)?;
    let encoding = sniff_encoding(&head);
    let extension = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    let json_by_extension = matches!(extension.as_str(), "json" | "jsonl" | "ndjson");
    let looks_like_json = head
        .iter()
        .find(|b| !b.is_ascii_whitespace())
        .is_some_and(|b| matches!(b, b'[' | b'{'));
    let format = if json_by_extension || looks_like_json {
        SourceFormat::Json(sniff_shape(&head))
    } else {
        SourceFormat::Csv(sniff_dialect(&head, encoding))
    };
    Ok(FileSniff { format, encoding })
}

/// How an empty CSV field becomes a column value — an explicit choice, never
/// a silent convention. See [`coerce_field`](super::coerce::coerce_field).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum EmptyField {
    /// An empty field is SQL NULL. The default, and what round-trips hubro's
    /// own CSV export, which writes NULL as an empty field.
    #[default]
    Null,
    /// An empty field is the empty string — for a NOT NULL text column whose
    /// blanks really are blanks.
    EmptyText,
}

impl EmptyField {
    pub fn label(self) -> &'static str {
        match self {
            EmptyField::Null => "empty fields become NULL",
            EmptyField::EmptyText => "empty fields become an empty string",
        }
    }
}

/// What a row hubro cannot coerce does to the import.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ErrorMode {
    /// Abort and roll back everything. The default: see the module docs.
    #[default]
    Abort,
    /// Skip the row, carry on, and report every skipped line.
    Skip,
}

/// One source field feeding one target column.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnBinding {
    pub source: SourceField,
    pub column: String,
}

/// Everything the import needs beyond the file and the table.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ImportOptions {
    pub mapping: Vec<ColumnBinding>,
    pub empty_field: EmptyField,
    pub on_error: ErrorMode,
}

/// One row that was skipped, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkippedRow {
    /// 1-based physical line the record started on.
    pub line: u64,
    pub reason: String,
}

/// What a completed import did.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ImportReport {
    pub inserted_rows: u64,
    /// Every skipped row, up to [`MAX_REPORTED_SKIPS`] of them.
    pub skipped: Vec<SkippedRow>,
    /// How many rows were skipped in total, which can exceed
    /// `skipped.len()`.
    pub skipped_rows: u64,
}

impl ImportReport {
    /// Whether the individual list is shorter than the count.
    pub fn skips_truncated(&self) -> bool {
        self.skipped_rows > self.skipped.len() as u64
    }
}

/// A failed import. **Nothing was written**: either the failure came before
/// any row was sent, or the transaction rolled the sent ones back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportError {
    /// The file line the failure is about, when it is about one.
    pub line: Option<u64>,
    pub message: String,
    /// How many rows this import had already inserted when it failed — every
    /// one of them rolled back.
    ///
    /// Not decoration: it is the difference between "the import stopped
    /// before doing anything" and "the import had written 400 rows and undid
    /// them", which is the whole safety claim, and a test can assert it is
    /// non-zero while the table is unchanged.
    pub undone_rows: u64,
}

impl ImportError {
    fn at(line: Option<u64>, message: impl Into<String>) -> ImportError {
        ImportError {
            line,
            message: message.into(),
            undone_rows: 0,
        }
    }
}

impl std::fmt::Display for ImportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.line {
            Some(line) => write!(f, "line {line}: {}", self.message),
            None => write!(f, "{}", self.message),
        }
    }
}

impl std::error::Error for ImportError {}

/// Why this object cannot be imported into, or `None` when it can.
///
/// One place, so the disabled button, the dialog's explanation and the
/// backstop inside [`run_import`] all say the same sentence — the pattern
/// capability gating uses. Two reasons, in order:
///
/// 1. anything that makes the object unwritable, including the user's own
///    read-only marking — an import is a write, and is refused
///    wherever editing is;
/// 2. a connection that cannot hold a transaction, because then the abort
///    guarantee this whole feature is built on would be a lie. Unreachable
///    through [`TableAccess::resolve`] today (a table on such a connection is
///    already unwritable for the same reason), and checked anyway: it is the
///    condition the guarantee actually depends on.
pub fn import_refusal(access: &TableAccess) -> Option<&'static str> {
    if !access.can_mutate() {
        return Some(access.read_only_notice().unwrap_or(caps::NO_MUTATE));
    }
    if !access.caps.transactions {
        return Some(caps::NO_GUARDED_WRITE);
    }
    None
}

/// A column the import may write to: not database-assigned, since
/// `GENERATED ALWAYS` columns reject an ordinary INSERT outright.
pub fn is_importable(column: &ColumnMeta) -> bool {
    column.generated != Generated::Always
}

/// The mapping to start from: every source field bound to the target column
/// of the same name, case-insensitively. Fields that match nothing are left
/// unbound (the file may carry columns this table doesn't have), and target
/// columns that nothing maps to are left out of the INSERT entirely, so the
/// database's own defaults apply.
///
/// A headerless CSV has no names to match on, so its fields bind positionally
/// to the table's importable columns instead.
pub fn default_mapping(table: &TableMeta, fields: &[SourceField]) -> Vec<ColumnBinding> {
    let named: Vec<(SourceField, Option<&str>)> = fields
        .iter()
        .map(|source| match source {
            SourceField::Key(key) => (source.clone(), Some(key.as_str())),
            SourceField::Index(_) => (source.clone(), None),
        })
        .collect();
    bind_by_name(table, &named)
}

/// Named field mapping for a CSV file *with* a header: the header names are
/// the field labels, but the fields themselves are still positional.
pub fn header_fields(names: &[String]) -> Vec<SourceField> {
    (0..names.len()).map(SourceField::Index).collect()
}

/// Named mapping for a header row: binds by name rather than by position, so
/// a file whose columns are in a different order than the table still lands
/// correctly.
pub fn mapping_from_header(table: &TableMeta, names: &[String]) -> Vec<ColumnBinding> {
    let named: Vec<(SourceField, Option<&str>)> = names
        .iter()
        .enumerate()
        .map(|(index, name)| (SourceField::Index(index), Some(name.as_str())))
        .collect();
    bind_by_name(table, &named)
}

/// The one matching rule behind [`default_mapping`] and
/// [`mapping_from_header`], so a CSV header and a JSON key spelled alike bind
/// alike — they used to be two copies, and only the CSV one trimmed.
///
/// A field with a name binds to the importable column of that name, compared
/// case-insensitively and ignoring surrounding whitespace (`" id "` is `id`:
/// spreadsheets pad headers, and no real column name begins or ends with a
/// space). A field without one binds to the importable column at its own
/// position. Either way a column is bound at most once, first field first, so
/// two fields spelled alike do not produce an INSERT that names the same
/// column twice.
fn bind_by_name(table: &TableMeta, fields: &[(SourceField, Option<&str>)]) -> Vec<ColumnBinding> {
    let importable: Vec<&ColumnMeta> = table.columns.iter().filter(|c| is_importable(c)).collect();
    let mut bindings = Vec::new();
    let mut taken: Vec<&str> = Vec::new();
    for (source, name) in fields {
        let matched = match (name, source) {
            (Some(name), _) => importable
                .iter()
                .find(|c| c.name.eq_ignore_ascii_case(name.trim())),
            (None, SourceField::Index(index)) => importable.get(*index),
            (None, SourceField::Key(_)) => None,
        };
        let Some(column) = matched.map(|c| c.name.as_str()) else {
            continue;
        };
        if taken.contains(&column) {
            continue;
        }
        taken.push(column);
        bindings.push(ColumnBinding {
            source: source.clone(),
            column: column.to_string(),
        });
    }
    bindings
}

/// The first `max_rows` records, for the dialog's preview.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct SourcePreview {
    /// The fields found, in order — what the mapping UI lists.
    pub fields: Vec<SourceField>,
    /// Display labels parallel to `fields`.
    pub labels: Vec<String>,
    /// The header names, when the file has some.
    pub header: Option<Vec<String>>,
    /// Preview rows, each parallel to `fields`.
    pub rows: Vec<Vec<String>>,
}

/// Reads up to `max_rows` records for the preview, collecting the fields they
/// carry. For JSON, whose records need not agree on their keys, every key
/// seen in the sample becomes a field.
pub fn preview(source: &mut dyn RecordSource, max_rows: usize) -> Result<SourcePreview, ReadError> {
    let header = source.field_names()?;
    let mut fields: Vec<SourceField> = header.as_deref().map(header_fields).unwrap_or_default();
    let mut records = Vec::new();
    while records.len() < max_rows {
        let Some(record) = source.next_record()? else {
            break;
        };
        for field in record.present_fields() {
            if !fields.contains(&field) {
                fields.push(field);
            }
        }
        records.push(record);
    }
    let labels = fields
        .iter()
        .map(|field| field.label(header.as_deref()))
        .collect();
    let rows = records
        .iter()
        .map(|record| {
            fields
                .iter()
                .map(|field| record.field(field).display())
                .collect()
        })
        .collect();
    Ok(SourcePreview {
        fields,
        labels,
        header,
        rows,
    })
}

/// One binding resolved against the table: the field to read and the column
/// to write, with the column's metadata in hand for coercion.
#[derive(Debug)]
struct PlannedColumn<'t> {
    source: SourceField,
    column: &'t ColumnMeta,
}

/// Checks the mapping against the table before anything is sent: unknown
/// columns, duplicates, database-assigned columns, and an empty mapping are
/// all reported here rather than as a SQL error halfway through a file.
fn plan<'t>(
    table: &'t TableMeta,
    mapping: &[ColumnBinding],
) -> Result<Vec<PlannedColumn<'t>>, ImportError> {
    if mapping.is_empty() {
        return Err(ImportError::at(
            None,
            "no fields are mapped to columns, so there is nothing to import",
        ));
    }
    let mut planned: Vec<PlannedColumn<'t>> = Vec::with_capacity(mapping.len());
    for binding in mapping {
        let Some(column) = table.columns.iter().find(|c| c.name == binding.column) else {
            return Err(ImportError::at(
                None,
                format!("this table has no column named \"{}\"", binding.column),
            ));
        };
        if !is_importable(column) {
            return Err(ImportError::at(
                None,
                format!(
                    "column \"{}\" is assigned by the database and can't be imported into",
                    column.name
                ),
            ));
        }
        if planned.iter().any(|p| p.column.name == column.name) {
            return Err(ImportError::at(
                None,
                format!("column \"{}\" is mapped more than once", column.name),
            ));
        }
        planned.push(PlannedColumn {
            source: binding.source.clone(),
            column,
        });
    }
    Ok(planned)
}

/// Bound-parameter ceiling per statement, per backend. Postgres allows 65535,
/// SQL Server 2100, SQLite's compiled default is well above 999 on any
/// current build — each is taken with room to spare, since the only cost of a
/// smaller batch is one more round trip.
fn max_params(dialect: Dialect) -> usize {
    match dialect {
        Dialect::Sqlite => 900,
        Dialect::Postgres => 60_000,
        Dialect::SqlServer => 2_000,
    }
}

/// Rows per INSERT: as many as the parameter ceiling allows, capped at 500 —
/// SQL Server refuses a `VALUES` list longer than 1000 rows, and a smaller
/// batch keeps the memory the import holds at any moment bounded and small.
fn batch_rows(dialect: Dialect, columns: usize) -> usize {
    (max_params(dialect) / columns.max(1)).clamp(1, 500)
}

/// One value position of a row bound for the INSERT.
#[derive(Debug, Clone, PartialEq)]
enum Cell {
    Value(Value),
    /// The column's own default — what [`null_cell`] sends where a NULL
    /// cannot be stored but the column says what to store instead.
    Default,
}

/// What a NULL headed for `column` is sent as, or why the row cannot be.
///
/// - A **nullable** column takes the NULL. Its default, if it has one, does
///   not apply: the file said "no value", and the column can hold exactly
///   that.
/// - A **NOT NULL column the database fills in** — a `DEFAULT`, a `serial`'s
///   `nextval(…)`, `GENERATED BY DEFAULT AS IDENTITY` — gets that value. A
///   NULL could not be stored there, so sending one only fails the statement
///   (measured on Postgres, SQL Server and SQLite alike), and a server-side
///   failure aborts the import in skip mode too. The default is what the
///   column itself says an absent value becomes.
/// - Any **other NOT NULL column** refuses the row here rather than at the
///   server, so skip mode can actually skip it — the most common thing wrong
///   with a messy export.
fn null_cell(column: &ColumnMeta) -> Result<Cell, String> {
    if column.nullable {
        Ok(Cell::Value(Value::Null))
    } else if column.is_auto_assigned() {
        Ok(Cell::Default)
    } else {
        Err(format!(
            "column \"{}\" can't be empty (it is NOT NULL and has no default)",
            column.name
        ))
    }
}

/// Whether `INSERT … VALUES (…)` takes the `DEFAULT` keyword in a value
/// position. Postgres and SQL Server do, in multi-row lists too; SQLite does
/// not (a syntax error), so there a defaulted column is left out of the
/// statement instead — see [`statement_runs`].
fn has_default_keyword(dialect: Dialect) -> bool {
    match dialect {
        Dialect::Postgres | Dialect::SqlServer => true,
        Dialect::Sqlite => false,
    }
}

/// Splits one batch into the row ranges that are each sent as one INSERT.
///
/// Where the dialect has a `DEFAULT` keyword the whole batch is one
/// statement. On SQLite a defaulted cell can only be expressed by leaving its
/// column out, and a column list is per statement, so a new statement starts
/// wherever the set of defaulted columns changes; a row with *every* column
/// defaulted is a statement of its own, since `DEFAULT VALUES` inserts
/// exactly one row. That costs a statement per change, which is cheap only
/// because SQLite has no round trip to pay for — the reason the servers get
/// the keyword instead.
fn statement_runs(dialect: Dialect, rows: &[Vec<Cell>]) -> Vec<std::ops::Range<usize>> {
    if rows.is_empty() {
        return Vec::new();
    }
    if has_default_keyword(dialect) {
        return std::iter::once(0..rows.len()).collect();
    }
    let defaulted =
        |row: &[Cell]| -> Vec<bool> { row.iter().map(|cell| *cell == Cell::Default).collect() };
    let mut runs = Vec::new();
    let mut start = 0;
    for index in 1..rows.len() {
        let previous = defaulted(&rows[index - 1]);
        let all_default = previous.iter().all(|d| *d);
        if all_default || previous != defaulted(&rows[index]) {
            runs.push(start..index);
            start = index;
        }
    }
    runs.push(start..rows.len());
    runs
}

/// Builds one multi-row `INSERT INTO t (cols) VALUES (…),(…)` for rows that
/// [`statement_runs`] grouped together.
///
/// Values render exactly as a staged insert's do — through
/// [`ParamSql`](super::staged::ParamSql), so NULLs are the inline literal
/// (never a bound text NULL, which Postgres rejects for a typed column) and
/// every other parameter carries its column's Postgres cast. One renderer, so
/// an imported value and a hand-edited one reach the same column the same
/// way.
///
/// A [`Cell::Default`] is the `DEFAULT` keyword where the dialect has one. On
/// SQLite its column is left out of the statement — the rows agree on which
/// columns those are — and a statement left with no columns at all is
/// `DEFAULT VALUES`.
fn insert_sql(
    table: &TableMeta,
    dialect: Dialect,
    planned: &[PlannedColumn<'_>],
    rows: &[Vec<Cell>],
) -> (String, Vec<Value>) {
    let target = qualified(table.schema.as_deref(), &table.name);
    let named: Vec<usize> = (0..planned.len())
        .filter(|&index| {
            has_default_keyword(dialect)
                || rows.first().is_none_or(|row| row[index] != Cell::Default)
        })
        .collect();
    if named.is_empty() {
        debug_assert_eq!(rows.len(), 1, "DEFAULT VALUES inserts exactly one row");
        return (format!("INSERT INTO {target} DEFAULT VALUES"), Vec::new());
    }
    let casts = cast_targets(table, dialect);
    let mut params = ParamSql::new(dialect);
    let names: Vec<String> = named
        .iter()
        .map(|&index| quote_ident(&planned[index].column.name))
        .collect();
    let mut tuples = Vec::with_capacity(rows.len());
    for row in rows {
        let rendered: Vec<String> = named
            .iter()
            .map(|&index| match &row[index] {
                Cell::Default => "DEFAULT".to_string(),
                Cell::Value(value) => {
                    let column = planned[index].column.name.as_str();
                    params.value_sql(value, casts.get(column).map(String::as_str))
                }
            })
            .collect();
        tuples.push(format!("({})", rendered.join(", ")));
    }
    let sql = format!(
        "INSERT INTO {target} ({}) VALUES {}",
        names.join(", "),
        tuples.join(", ")
    );
    (sql, params.into_values())
}

/// Coerces one record into the cells for the planned columns, or reports the
/// first reason it cannot.
fn coerce_row(
    planned: &[PlannedColumn<'_>],
    dialect: Dialect,
    record: &Record,
    empty: EmptyField,
) -> Result<Vec<Cell>, String> {
    let mut row = Vec::with_capacity(planned.len());
    for slot in planned {
        let field = record.field(&slot.source);
        let value = coerce_field(slot.column, dialect, &field, empty)?;
        row.push(if value.is_null() {
            null_cell(slot.column)?
        } else {
            Cell::Value(value)
        });
    }
    Ok(row)
}

/// Streams `source` into `table`, one batch of rows per statement, all inside
/// one transaction.
///
/// See the module docs for the abort/skip contract. On success every row that
/// was not skipped is committed; on failure nothing is, and
/// [`ImportError::undone_rows`] reports how much was rolled back.
///
/// `access` is the connection's capabilities resolved for `table`
/// *including* the user's write protection, and is checked before
/// anything is read — it is passed in rather than re-derived so this backstop
/// and the UI's gate cannot answer differently.
pub async fn run_import(
    pool: &DbPool,
    access: &TableAccess,
    table: &TableMeta,
    options: &ImportOptions,
    source: &mut dyn RecordSource,
) -> Result<ImportReport, ImportError> {
    if let Some(reason) = import_refusal(access) {
        return Err(ImportError::at(
            None,
            DbError::Unsupported(reason.into()).to_string(),
        ));
    }
    let planned = plan(table, &options.mapping)?;
    let dialect = pool.dialect();
    let per_batch = batch_rows(dialect, planned.len());

    let mut tx = pool
        .begin_script_tx()
        .await
        .map_err(|e| ImportError::at(None, e.to_string()))?;
    let mut report = ImportReport::default();
    let mut batch: Vec<Vec<Cell>> = Vec::with_capacity(per_batch);

    loop {
        // Reading is fallible too, and a malformed file is not a row that can
        // be skipped: the parser no longer knows where the next record
        // starts.
        let next = match source.next_record() {
            Ok(next) => next,
            Err(err) => return Err(abort(tx, report.inserted_rows, err.line, err.message).await),
        };
        let Some(record) = next else { break };
        match coerce_row(&planned, dialect, &record, options.empty_field) {
            Ok(row) => batch.push(row),
            Err(reason) => match options.on_error {
                ErrorMode::Abort => {
                    return Err(abort(tx, report.inserted_rows, Some(record.line), reason).await)
                }
                ErrorMode::Skip => {
                    report.skipped_rows += 1;
                    if report.skipped.len() < MAX_REPORTED_SKIPS {
                        report.skipped.push(SkippedRow {
                            line: record.line,
                            reason,
                        });
                    }
                    continue;
                }
            },
        }
        if batch.len() >= per_batch {
            if let Err((err, applied)) = flush(&mut tx, table, dialect, &planned, &batch).await {
                let undone = report.inserted_rows + applied;
                return Err(abort(tx, undone, None, err.to_string()).await);
            }
            report.inserted_rows += batch.len() as u64;
            batch.clear();
        }
    }
    if !batch.is_empty() {
        if let Err((err, applied)) = flush(&mut tx, table, dialect, &planned, &batch).await {
            let undone = report.inserted_rows + applied;
            return Err(abort(tx, undone, None, err.to_string()).await);
        }
        report.inserted_rows += batch.len() as u64;
    }
    tx.commit().await.map_err(|e| ImportError {
        line: None,
        message: e.to_string(),
        undone_rows: report.inserted_rows,
    })?;
    Ok(report)
}

/// Sends one batch, holding every statement to the row count it should have
/// affected — the same guard every staged write runs under
/// ([`execute_all_checked`](DbPool::execute_all_checked)): a statement that
/// affected a different number of rows than it had values for has done
/// something nobody asked for, and must not be committed.
///
/// A batch is one statement except on SQLite, where [`statement_runs`] may
/// split it.
async fn flush(
    tx: &mut ScriptTx<'_>,
    table: &TableMeta,
    dialect: Dialect,
    planned: &[PlannedColumn<'_>],
    rows: &[Vec<Cell>],
) -> Result<(), (DbError, u64)> {
    // The error carries how many rows of THIS batch had landed, so
    // [`ImportError::undone_rows`] counts everything the rollback undid —
    // earlier statements of the batch included. The row-count mismatch is
    // the one case where the failing statement's own share is not zero — and
    // it was the case the count used to miss, which is precisely the case the
    // guard exists for.
    let mut applied = 0u64;
    for run in statement_runs(dialect, rows) {
        let rows = &rows[run];
        let (sql, params) = insert_sql(table, dialect, planned, rows);
        let affected = tx
            .execute_with(&sql, &params)
            .await
            .map_err(|err| (err, applied))?;
        let expected = rows.len() as u64;
        if affected != expected {
            return Err((
                DbError::row_count_mismatch(affected, expected),
                applied + affected,
            ));
        }
        applied += affected;
    }
    Ok(())
}

/// Rolls the import back and builds the error that says so.
async fn abort(
    tx: ScriptTx<'_>,
    inserted: u64,
    line: Option<u64>,
    message: impl Into<String>,
) -> ImportError {
    tx.rollback().await;
    ImportError {
        line,
        message: message.into(),
        undone_rows: inserted,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::caps::{Capabilities, Restriction, WriteProtection};
    use crate::db::schema::{TableKind, TypeDetail};

    fn col(name: &str, type_name: &str, nullable: bool) -> ColumnMeta {
        ColumnMeta {
            name: name.into(),
            type_name: type_name.into(),
            nullable,
            primary_key_position: None,
            default: None,
            generated: Generated::Never,
            type_detail: TypeDetail::Plain,
        }
    }

    fn table(columns: Vec<ColumnMeta>) -> TableMeta {
        TableMeta {
            schema: Some("public".into()),
            name: "people".into(),
            kind: TableKind::Table,
            columns,
            indexes: vec![],
            foreign_keys: vec![],
            restriction: None,
            internal: None,
            kind_label: None,
        }
    }

    fn people() -> TableMeta {
        let mut id = col("id", "integer", false);
        id.primary_key_position = Some(1);
        table(vec![
            id,
            col("name", "text", false),
            col("age", "integer", true),
        ])
    }

    fn bind(index: usize, column: &str) -> ColumnBinding {
        ColumnBinding {
            source: SourceField::Index(index),
            column: column.to_string(),
        }
    }

    fn csv_source(text: &'static str, has_header: bool) -> CsvReader<&'static [u8]> {
        CsvReader::new(
            text.as_bytes(),
            CsvDialect {
                has_header,
                ..CsvDialect::default()
            },
            Encoding::Utf8,
        )
    }

    #[test]
    fn an_import_is_refused_wherever_editing_is() {
        let people = people();
        // A writable table on a full backend: nothing to refuse.
        let access = TableAccess::resolve(Capabilities::FULL, &people, Dialect::Postgres);
        assert_eq!(import_refusal(&access), None);

        // A read-only connection, a view, and the user's own marking each
        // refuse with their own sentence — the same one the disabled Save
        // button shows.
        let read_only =
            TableAccess::resolve(Capabilities::FULL.read_only(), &people, Dialect::Postgres);
        assert_eq!(import_refusal(&read_only), Some(caps::NO_MUTATE));

        let marked = TableAccess::resolve_protected(
            Capabilities::FULL,
            WriteProtection::ReadOnly,
            &people,
            Dialect::Postgres,
        );
        assert_eq!(import_refusal(&marked), Some(caps::MARKED_READ_ONLY));

        let mut view = people.clone();
        view.kind = TableKind::View;
        let view_access = TableAccess::resolve(Capabilities::FULL, &view, Dialect::Postgres);
        assert_eq!(
            import_refusal(&view_access),
            Some(Restriction::View.message())
        );
    }

    #[test]
    fn an_import_is_refused_without_a_transaction_to_undo_it() {
        // The condition the abort guarantee actually rests on. `resolve`
        // cannot produce a writable table on a transaction-less connection
        // today (it refuses editing there for the same reason), so the set is
        // built directly — a guard that can only be reached through a
        // synthetic input is still the guard that has to hold.
        let access = TableAccess {
            caps: Capabilities {
                transactions: false,
                ..Capabilities::FULL
            },
            identity: None,
            restriction: None,
            unreadable: None,
        };
        assert!(access.can_mutate());
        assert_eq!(import_refusal(&access), Some(caps::NO_GUARDED_WRITE));
    }

    #[test]
    fn the_mapping_is_checked_against_the_table_before_anything_is_sent() {
        let people = people();
        assert!(plan(&people, &[]).is_err());

        let unknown = plan(&people, &[bind(0, "nope")]).unwrap_err();
        assert!(unknown.message.contains("no column named"), "{unknown}");

        let duplicate = plan(&people, &[bind(0, "name"), bind(1, "name")]).unwrap_err();
        assert!(duplicate.message.contains("more than once"), "{duplicate}");

        let mut generated = people.clone();
        generated.columns[0].generated = Generated::Always;
        let refused = plan(&generated, &[bind(0, "id")]).unwrap_err();
        assert!(
            refused.message.contains("assigned by the database"),
            "{refused}"
        );

        // Nothing was attempted, so nothing was undone.
        assert_eq!(unknown.undone_rows, 0);
    }

    #[test]
    fn the_default_mapping_matches_names_case_insensitively() {
        let people = people();
        let names: Vec<String> = vec!["Name".into(), "AGE".into(), "unrelated".into()];
        let mapping = mapping_from_header(&people, &names);
        assert_eq!(
            mapping,
            vec![bind(0, "name"), bind(1, "age")],
            "a field matching no column binds nothing"
        );

        // A header in a different order than the table still lands right.
        let reordered = mapping_from_header(&people, &["age".to_string(), "id".into()]);
        assert_eq!(reordered, vec![bind(0, "age"), bind(1, "id")]);
    }

    #[test]
    fn a_json_key_and_a_csv_header_spelled_alike_bind_alike() {
        // One rule for both: case-insensitive, surrounding whitespace
        // ignored. Only the CSV path used to trim, so a padded JSON key
        // silently bound nothing.
        let people = people();
        let spellings = [" id ", "NAME\t", "Age"];
        let header: Vec<String> = spellings.iter().map(|s| s.to_string()).collect();
        assert_eq!(
            mapping_from_header(&people, &header),
            vec![bind(0, "id"), bind(1, "name"), bind(2, "age")]
        );
        let keys: Vec<SourceField> = spellings
            .iter()
            .map(|s| SourceField::Key(s.to_string()))
            .collect();
        let by_key = default_mapping(&people, &keys);
        assert_eq!(
            by_key
                .iter()
                .map(|b| (b.source.clone(), b.column.as_str()))
                .collect::<Vec<_>>(),
            vec![
                (keys[0].clone(), "id"),
                (keys[1].clone(), "name"),
                (keys[2].clone(), "age")
            ]
        );
        // And both bind a column once, first spelling first.
        let twice = vec![
            SourceField::Key("name".into()),
            SourceField::Key(" Name".into()),
        ];
        assert_eq!(default_mapping(&people, &twice).len(), 1);
        assert_eq!(
            mapping_from_header(&people, &["name".to_string(), " Name".into()]),
            vec![bind(0, "name")]
        );
    }

    #[test]
    fn a_headerless_file_binds_positionally() {
        let people = people();
        let fields = vec![SourceField::Index(0), SourceField::Index(1)];
        assert_eq!(
            default_mapping(&people, &fields),
            vec![bind(0, "id"), bind(1, "name")]
        );
    }

    #[test]
    fn a_generated_always_column_is_never_bound_by_default() {
        let mut people = people();
        people.columns[0].generated = Generated::Always;
        let fields = vec![SourceField::Index(0), SourceField::Index(1)];
        // Position 0 now addresses "name": the database-assigned column is
        // not a target at all.
        assert_eq!(
            default_mapping(&people, &fields),
            vec![bind(0, "name"), bind(1, "age")]
        );
    }

    #[test]
    fn rows_are_coerced_against_their_columns() {
        let people = people();
        let planned = plan(&people, &[bind(0, "id"), bind(1, "name"), bind(2, "age")]).unwrap();
        let record = Record::positional(7, vec!["1".into(), "ada".into(), "36".into()]);
        assert_eq!(
            coerce_row(&planned, Dialect::Postgres, &record, EmptyField::Null).unwrap(),
            vec![
                Cell::Value(Value::Integer(1)),
                Cell::Value(Value::Text("ada".into())),
                Cell::Value(Value::Integer(36))
            ]
        );

        // An empty optional field is NULL; an empty NOT NULL one is the error
        // skip mode exists for, and it names the column.
        let sparse = Record::positional(8, vec!["2".into(), "".into(), "".into()]);
        let err = coerce_row(&planned, Dialect::Postgres, &sparse, EmptyField::Null).unwrap_err();
        assert!(err.contains("\"name\""), "{err}");
        assert!(err.contains("NOT NULL"), "{err}");

        // ...and with empty-as-text the text column takes it, which is the
        // whole point of the option being explicit.
        let text_only = plan(&people, &[bind(0, "id"), bind(1, "name")]).unwrap();
        let row = coerce_row(
            &text_only,
            Dialect::Postgres,
            &sparse,
            EmptyField::EmptyText,
        )
        .unwrap();
        assert_eq!(row[1], Cell::Value(Value::Text(String::new())));

        // But an empty field for a *number* is refused under that setting
        // rather than quietly becoming NULL anyway — the option would
        // otherwise be a lie exactly where someone goes looking for it.
        let empty_number =
            coerce_row(&planned, Dialect::Postgres, &sparse, EmptyField::EmptyText).unwrap_err();
        assert!(empty_number.contains("\"age\""), "{empty_number}");
        assert!(empty_number.contains("not a number"), "{empty_number}");
    }

    #[test]
    fn a_short_record_leaves_its_missing_columns_null() {
        let people = people();
        let planned = plan(&people, &[bind(0, "id"), bind(2, "age")]).unwrap();
        let record = Record::positional(3, vec!["1".into()]);
        assert_eq!(
            coerce_row(&planned, Dialect::Postgres, &record, EmptyField::Null).unwrap(),
            vec![Cell::Value(Value::Integer(1)), Cell::Value(Value::Null)]
        );
    }

    /// `people` plus the three kinds of column a NULL can head for besides a
    /// plain nullable one.
    fn with_defaults() -> TableMeta {
        let mut people = people();
        let mut qty = col("qty", "integer", false);
        qty.default = Some("7".into());
        let mut seq = col("seq", "integer", false);
        seq.generated = Generated::ByDefault;
        let mut note = col("note", "text", true);
        note.default = Some("'none'".into());
        people.columns.extend([qty, seq, note]);
        people
    }

    fn column<'t>(table: &'t TableMeta, name: &str) -> &'t ColumnMeta {
        table.columns.iter().find(|c| c.name == name).unwrap()
    }

    #[test]
    fn a_null_becomes_the_default_only_where_null_cannot_be_stored() {
        let table = with_defaults();
        // NOT NULL with a default, and NOT NULL filled by an identity: the
        // column's own value, never a NULL the server refuses.
        assert_eq!(null_cell(column(&table, "qty")), Ok(Cell::Default));
        assert_eq!(null_cell(column(&table, "seq")), Ok(Cell::Default));
        // Nullable, with or without a default: the NULL the file said.
        assert_eq!(
            null_cell(column(&table, "age")),
            Ok(Cell::Value(Value::Null))
        );
        assert_eq!(
            null_cell(column(&table, "note")),
            Ok(Cell::Value(Value::Null))
        );
        // NOT NULL with nothing to fill it: refused by hubro, so skippable.
        let err = null_cell(column(&table, "name")).unwrap_err();
        assert!(err.contains("\"name\""), "{err}");
        assert!(err.contains("no default"), "{err}");
    }

    #[test]
    fn an_empty_or_missing_field_for_a_defaulted_not_null_column_is_sent_as_default() {
        // The wire from the file to the cell: an empty CSV field and an
        // absent JSON key both reach `null_cell` and come out as the default.
        let table = with_defaults();
        let planned = plan(&table, &[bind(0, "id"), bind(1, "qty"), bind(2, "seq")]).unwrap();
        let blank = Record::positional(4, vec!["1".into(), "".into(), "".into()]);
        assert_eq!(
            coerce_row(&planned, Dialect::Postgres, &blank, EmptyField::Null).unwrap(),
            vec![Cell::Value(Value::Integer(1)), Cell::Default, Cell::Default]
        );

        let keyed = plan(
            &table,
            &[
                ColumnBinding {
                    source: SourceField::Key("id".into()),
                    column: "id".into(),
                },
                ColumnBinding {
                    source: SourceField::Key("qty".into()),
                    column: "qty".into(),
                },
            ],
        )
        .unwrap();
        let mut map = serde_json::Map::new();
        map.insert("id".into(), serde_json::json!(2));
        let missing = Record::keyed(5, map);
        assert_eq!(
            coerce_row(&keyed, Dialect::Postgres, &missing, EmptyField::Null).unwrap(),
            vec![Cell::Value(Value::Integer(2)), Cell::Default]
        );
    }

    #[test]
    fn a_defaulted_cell_is_the_default_keyword_on_the_servers() {
        let table = with_defaults();
        let planned = plan(&table, &[bind(0, "id"), bind(1, "qty")]).unwrap();
        let rows = vec![
            vec![Cell::Value(Value::Integer(1)), Cell::Default],
            vec![
                Cell::Value(Value::Integer(2)),
                Cell::Value(Value::Integer(3)),
            ],
        ];
        assert_eq!(statement_runs(Dialect::Postgres, &rows), vec![0..2]);
        assert_eq!(statement_runs(Dialect::SqlServer, &rows), vec![0..2]);
        let (sql, params) = insert_sql(&table, Dialect::Postgres, &planned, &rows);
        assert_eq!(
            sql,
            "INSERT INTO \"public\".\"people\" (\"id\", \"qty\") \
             VALUES ($1::integer, DEFAULT), ($2::integer, $3::integer)"
        );
        assert_eq!(
            params,
            vec![Value::Integer(1), Value::Integer(2), Value::Integer(3)]
        );
        let (mssql, _) = insert_sql(&table, Dialect::SqlServer, &planned, &rows);
        assert!(
            mssql.ends_with("VALUES (@P1, DEFAULT), (@P2, @P3)"),
            "{mssql}"
        );
    }

    #[test]
    fn sqlite_leaves_a_defaulted_column_out_of_its_statement() {
        let table = with_defaults();
        let planned = plan(&table, &[bind(0, "id"), bind(1, "qty")]).unwrap();
        let int = |n| Cell::Value(Value::Integer(n));
        let rows = vec![
            vec![int(1), Cell::Default],
            vec![int(2), Cell::Default],
            vec![int(3), int(30)],
            vec![Cell::Default, Cell::Default],
            vec![Cell::Default, Cell::Default],
            vec![int(6), Cell::Default],
        ];
        // A statement per change of defaulted columns, and an all-default
        // row on its own: `DEFAULT VALUES` inserts exactly one row.
        let runs = statement_runs(Dialect::Sqlite, &rows);
        assert_eq!(runs, vec![0..2, 2..3, 3..4, 4..5, 5..6]);

        let sql =
            |run: std::ops::Range<usize>| insert_sql(&table, Dialect::Sqlite, &planned, &rows[run]);
        assert_eq!(
            sql(0..2),
            (
                "INSERT INTO \"public\".\"people\" (\"id\") VALUES (?), (?)".to_string(),
                vec![Value::Integer(1), Value::Integer(2)]
            )
        );
        assert!(sql(2..3).0.ends_with("(\"id\", \"qty\") VALUES (?, ?)"));
        assert_eq!(
            sql(3..4),
            (
                "INSERT INTO \"public\".\"people\" DEFAULT VALUES".to_string(),
                vec![]
            )
        );
        // Never the keyword, which SQLite rejects as a syntax error.
        for run in runs {
            let (text, _) = sql(run);
            assert!(
                text.ends_with("DEFAULT VALUES") || !text.contains("DEFAULT"),
                "{text}"
            );
        }
    }

    #[test]
    fn one_statement_carries_the_whole_batch_with_its_values_bound() {
        let people = people();
        let planned = plan(&people, &[bind(0, "id"), bind(1, "name")]).unwrap();
        let rows = vec![
            vec![
                Cell::Value(Value::Integer(1)),
                Cell::Value(Value::Text("ada".into())),
            ],
            vec![Cell::Value(Value::Integer(2)), Cell::Value(Value::Null)],
        ];
        let (sql, params) = insert_sql(&people, Dialect::Postgres, &planned, &rows);
        assert_eq!(
            sql,
            "INSERT INTO \"public\".\"people\" (\"id\", \"name\") \
             VALUES ($1::integer, $2::text), ($3::integer, NULL)"
        );
        // NULL is the inline literal, never a bound parameter — Postgres
        // rejects a text-typed NULL for a typed column.
        assert_eq!(
            params,
            vec![
                Value::Integer(1),
                Value::Text("ada".into()),
                Value::Integer(2)
            ]
        );

        // SQLite numbers nothing and casts nothing.
        let (sqlite_sql, _) = insert_sql(&people, Dialect::Sqlite, &planned, &rows);
        assert!(
            sqlite_sql.ends_with("VALUES (?, ?), (?, NULL)"),
            "{sqlite_sql}"
        );
        // SQL Server uses its own placeholder spelling.
        let (mssql_sql, _) = insert_sql(&people, Dialect::SqlServer, &planned, &rows);
        assert!(mssql_sql.contains("(@P1, @P2)"), "{mssql_sql}");
    }

    #[test]
    fn batches_stay_under_each_backends_parameter_ceiling() {
        for dialect in [Dialect::Sqlite, Dialect::Postgres, Dialect::SqlServer] {
            for columns in [1usize, 3, 17, 400] {
                let rows = batch_rows(dialect, columns);
                assert!(rows >= 1, "{dialect:?}/{columns}");
                assert!(rows <= 500, "{dialect:?}/{columns}");
                assert!(
                    rows * columns <= max_params(dialect),
                    "{dialect:?}/{columns}: {rows} rows would bind {} parameters",
                    rows * columns
                );
            }
        }
        // A table wider than the ceiling still sends one row at a time rather
        // than dividing to zero.
        assert_eq!(batch_rows(Dialect::SqlServer, 5_000), 1);
    }

    #[test]
    fn a_csv_file_previews_its_header_and_first_rows() {
        let mut source = csv_source("id,name\n1,ada\n2,grace\n3,alan\n", true);
        let preview = preview(&mut source, 2).unwrap();
        assert_eq!(preview.header, Some(vec!["id".into(), "name".into()]));
        assert_eq!(preview.labels, vec!["id".to_string(), "name".into()]);
        assert_eq!(
            preview.rows,
            vec![
                vec!["1".to_string(), "ada".into()],
                vec!["2".to_string(), "grace".into()]
            ]
        );
    }

    #[test]
    fn a_json_preview_collects_every_key_the_sample_carries() {
        let mut source = JsonReader::new(
            r#"[{"a":1},{"b":2},{"a":3,"b":4}]"#.as_bytes(),
            JsonShape::Array,
            Encoding::Utf8,
        );
        let preview = preview(&mut source, 10).unwrap();
        assert_eq!(preview.header, None);
        assert_eq!(preview.labels, vec!["a".to_string(), "b".into()]);
        // A record without a key shows an empty cell there.
        assert_eq!(preview.rows[0], vec!["1".to_string(), "".into()]);
        assert_eq!(preview.rows[1], vec!["".to_string(), "2".into()]);
    }

    #[test]
    fn a_json_preview_lists_keys_in_the_order_the_file_has_them() {
        // `serde_json::Map` is a BTreeMap unless `preserve_order` is on, and
        // nothing else in the build turns it on — so the preview used to list
        // every JSON file's columns alphabetically.
        let mut source = JsonReader::new(
            r#"[{"zeta":1,"alpha":2,"mid":3},{"beta":4}]"#.as_bytes(),
            JsonShape::Array,
            Encoding::Utf8,
        );
        let preview = preview(&mut source, 10).unwrap();
        assert_eq!(preview.labels, vec!["zeta", "alpha", "mid", "beta"]);
    }

    #[test]
    fn the_skip_list_is_capped_but_the_count_is_not() {
        let report = ImportReport {
            inserted_rows: 1,
            skipped: vec![
                SkippedRow {
                    line: 2,
                    reason: "nope".into(),
                };
                MAX_REPORTED_SKIPS
            ],
            skipped_rows: MAX_REPORTED_SKIPS as u64 + 5,
        };
        assert!(report.skips_truncated());
    }

    #[test]
    fn sniffing_reads_the_encoding_off_the_bytes() {
        assert_eq!(sniff_encoding("plain,ascii\n".as_bytes()), Encoding::Utf8);
        assert_eq!(sniff_encoding("héllo\n".as_bytes()), Encoding::Utf8);
        // 0xE9 alone is not valid UTF-8.
        assert_eq!(sniff_encoding(&[b'a', 0xE9, b'\n']), Encoding::Latin1);
        // A sample cut mid-character is not evidence of Latin-1.
        let cut = "é".as_bytes()[..1].to_vec();
        assert_eq!(sniff_encoding(&cut), Encoding::Utf8);
    }
}
