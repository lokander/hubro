//! Importing a CSV or JSON file into an **existing** table (FRE-112) — the
//! inverse of [`export`](super::export), and built the same way: records are
//! pulled from the file ONE AT A TIME and written in batches, so peak memory
//! is one batch regardless of file size.
//!
//! # What it will not do
//!
//! Create a table. The target's [`ColumnMeta`] is what supplies the types, so
//! there is nothing to infer and nothing to guess; inferring a schema from a
//! file is FRE-122's problem.
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

    /// The fields this record actually carries, in order.
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
/// backstop inside [`run_import`] all say the same sentence — the FRE-87
/// pattern. Two reasons, in order:
///
/// 1. anything that makes the object unwritable, including the user's own
///    read-only marking (FRE-111) — an import is a write, and is refused
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
    let importable: Vec<&ColumnMeta> = table.columns.iter().filter(|c| is_importable(c)).collect();
    let mut bindings = Vec::new();
    let mut taken: Vec<&str> = Vec::new();
    for source in fields {
        // A positional field takes the importable column at its own index; a
        // named one matches by name. Either way a column is bound at most
        // once, so two fields spelled alike do not produce an INSERT that
        // names the same column twice.
        let matched = match source {
            SourceField::Key(key) => importable
                .iter()
                .find(|c| c.name.eq_ignore_ascii_case(key))
                .map(|c| c.name.as_str()),
            SourceField::Index(index) => importable.get(*index).map(|c| c.name.as_str()),
        };
        if let Some(name) = matched {
            if taken.contains(&name) {
                continue;
            }
            taken.push(name);
            bindings.push(ColumnBinding {
                source: source.clone(),
                column: name.to_string(),
            });
        }
    }
    bindings
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
    let importable: Vec<&ColumnMeta> = table.columns.iter().filter(|c| is_importable(c)).collect();
    let mut bindings = Vec::new();
    let mut taken: Vec<&str> = Vec::new();
    for (index, name) in names.iter().enumerate() {
        let matched = importable
            .iter()
            .find(|c| c.name.eq_ignore_ascii_case(name.trim()))
            .map(|c| c.name.as_str());
        if let Some(column) = matched {
            if taken.contains(&column) {
                continue;
            }
            taken.push(column);
            bindings.push(ColumnBinding {
                source: SourceField::Index(index),
                column: column.to_string(),
            });
        }
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

/// Builds one multi-row `INSERT INTO t (cols) VALUES (…),(…)`.
///
/// Values render exactly as a staged insert's do — through
/// [`ParamSql`](super::staged::ParamSql), so NULLs are the inline literal
/// (never a bound text NULL, which Postgres rejects for a typed column) and
/// every other parameter carries its column's Postgres cast. One renderer, so
/// an imported value and a hand-edited one reach the same column the same
/// way.
fn insert_sql(
    table: &TableMeta,
    dialect: Dialect,
    planned: &[PlannedColumn<'_>],
    rows: &[Vec<Value>],
) -> (String, Vec<Value>) {
    let casts = cast_targets(table, dialect);
    let mut params = ParamSql::new(dialect);
    let names: Vec<String> = planned
        .iter()
        .map(|p| quote_ident(&p.column.name))
        .collect();
    let mut tuples = Vec::with_capacity(rows.len());
    for row in rows {
        let rendered: Vec<String> = row
            .iter()
            .zip(planned)
            .map(|(value, planned)| {
                let cast = casts.get(planned.column.name.as_str()).map(String::as_str);
                params.value_sql(value, cast)
            })
            .collect();
        tuples.push(format!("({})", rendered.join(", ")));
    }
    let sql = format!(
        "INSERT INTO {} ({}) VALUES {}",
        qualified(table.schema.as_deref(), &table.name),
        names.join(", "),
        tuples.join(", ")
    );
    (sql, params.into_values())
}

/// Coerces one record into the values for the planned columns, or reports the
/// first reason it cannot.
fn coerce_row(
    planned: &[PlannedColumn<'_>],
    dialect: Dialect,
    record: &Record,
    empty: EmptyField,
) -> Result<Vec<Value>, String> {
    let mut row = Vec::with_capacity(planned.len());
    for slot in planned {
        let field = record.field(&slot.source);
        let value = coerce_field(slot.column, dialect, &field, empty)?;
        // A NULL heading for a column that forbids one is caught here rather
        // than by the server, so skip mode can actually skip it — the most
        // common thing wrong with a messy export.
        if value.is_null() && !slot.column.nullable && !slot.column.is_auto_assigned() {
            return Err(format!(
                "column \"{}\" can't be empty (it is NOT NULL and has no default)",
                slot.column.name
            ));
        }
        row.push(value);
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
/// `access` is the connection's capabilities resolved for `table` (FRE-87)
/// *including* the user's write protection (FRE-111), and is checked before
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
    let mut batch: Vec<Vec<Value>> = Vec::with_capacity(per_batch);

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

/// Sends one batch, holding it to the row count it should have affected —
/// the same guard every staged write runs under
/// ([`execute_all_checked`](DbPool::execute_all_checked)): a statement that
/// affected a different number of rows than it had values for has done
/// something nobody asked for, and must not be committed.
async fn flush(
    tx: &mut ScriptTx<'_>,
    table: &TableMeta,
    dialect: Dialect,
    planned: &[PlannedColumn<'_>],
    rows: &[Vec<Value>],
) -> Result<(), (DbError, u64)> {
    let (sql, params) = insert_sql(table, dialect, planned, rows);
    // The error carries how many rows of THIS batch had landed, so
    // [`ImportError::undone_rows`] counts everything the rollback undid. The
    // row-count mismatch is the one case where that is not zero — and it was
    // the case the count used to miss, which is precisely the case the guard
    // exists for.
    let affected = tx
        .execute_with(&sql, &params)
        .await
        .map_err(|err| (err, 0))?;
    let expected = rows.len() as u64;
    if affected != expected {
        return Err((DbError::row_count_mismatch(affected, expected), affected));
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
                Value::Integer(1),
                Value::Text("ada".into()),
                Value::Integer(36)
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
        assert_eq!(row[1], Value::Text(String::new()));

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
            vec![Value::Integer(1), Value::Null]
        );
    }

    #[test]
    fn one_statement_carries_the_whole_batch_with_its_values_bound() {
        let people = people();
        let planned = plan(&people, &[bind(0, "id"), bind(1, "name")]).unwrap();
        let rows = vec![
            vec![Value::Integer(1), Value::Text("ada".into())],
            vec![Value::Integer(2), Value::Null],
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
