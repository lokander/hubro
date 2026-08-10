//! Streaming CSV reading for imports (FRE-112) — the inverse of the CSV
//! writing in [`export`](crate::db::export), and hand-rolled for the same
//! reason: the writer is, the rules are RFC-4180's, and the reader needs two
//! things a general-purpose crate would not give it for free — a manual
//! delimiter/quote override and a non-UTF-8 [`Encoding`].
//!
//! [`CsvReader`] pulls **one record at a time** from any [`BufRead`], so peak
//! memory is one record — bounded by [`MAX_FIELD_BYTES`] and
//! [`MAX_RECORD_BYTES`] even when the file is malformed — regardless of file
//! size, mirroring how the export streams one row at a time.
//!
//! Parsing follows RFC-4180 with the tolerances real files need:
//!
//! - a field is quoted or it is not; inside quotes a doubled quote is one
//!   literal quote, and a delimiter, CR or LF is ordinary text;
//! - line terminators are LF or CRLF, and a CRLF's CR belongs to the
//!   terminator wherever it appears — including at the end of the last line,
//!   with or without its LF;
//! - a completely blank line yields no record, so the newline that ends the
//!   last line never produces a phantom all-empty row. **CRLF included**: a CR
//!   is not content, so `\r\n\r\n` is one blank line and not a record of one
//!   empty field. (It used to be, which put an all-NULL row into the table for
//!   every Excel export ending in a blank line, or aborted the import against
//!   a NOT NULL column at a line with no data on it.);
//! - text after a closing quote (`"a"b`) is kept verbatim rather than
//!   rejected, because refusing the whole file over one sloppy field would be
//!   the worse failure — the value that reaches the column is `ab`;
//! - an unterminated quote *is* an error: everything after it would silently
//!   collapse into one field, and reporting the line beats importing that.
//!   The error arrives at [`MAX_FIELD_BYTES`] rather than at end of file, so
//!   the memory bound holds for a malformed file too — reading a 2 GB file
//!   into one field to discover the quote never closed is the same bug the
//!   error exists to prevent, one level down.
//!
//! Field text is decoded per [`Encoding`], so a Latin-1 file (which is not
//! valid UTF-8 and would otherwise fail outright) imports by choosing it.

use std::io::{self, BufRead};

use super::{decode_bytes, Encoding, ReadError, Record, RecordSource};

/// Wraps an I/O failure while reading the file, naming the line reached.
fn io_read_error(line: u64, err: io::Error) -> ReadError {
    ReadError {
        line: Some(line),
        message: format!("reading the file failed: {err}"),
    }
}

/// The error for a quoted field that runs past the end of the file.
fn unclosed_quote(line: u64) -> ReadError {
    ReadError {
        line: Some(line),
        message: "a quoted field is never closed before the end of the file".to_string(),
    }
}

/// The error for a field or record past its size bound. Names the quote
/// character when the reader was inside a quoted field, because that is
/// overwhelmingly the cause: the wrong quote character turns the rest of the
/// file into one value.
fn oversized(line: u64, in_quotes: bool, what: &str, limit: usize) -> ReadError {
    let hint = if in_quotes {
        " — an opening quote here is never closed, so check the quote character"
    } else {
        " — check the delimiter, or that this is really a CSV file"
    };
    ReadError {
        line: Some(line),
        message: format!(
            "this {what} is larger than {} MiB{hint}",
            limit / (1024 * 1024)
        ),
    }
}

/// Delimiters [`sniff_dialect`] considers, in preference order — comma first
/// so a file with no evidence either way stays plain CSV.
pub const DELIMITERS: [u8; 4] = *b",;\t|";

/// How many records the sniffers look at. Enough to see a shape, small enough
/// that detection stays instant on a huge file.
const SNIFF_RECORDS: usize = 20;

/// How much of a file [`super::sniff_file`] reads to detect with.
pub const SNIFF_BYTES: usize = 64 * 1024;

/// Largest single field the reader will assemble before giving up.
///
/// This is what keeps the streaming guarantee true for a *malformed* file: a
/// quote that never closes, or a quote character set to something the file
/// uses as ordinary text, otherwise swallows every remaining byte into one
/// field. Matching
/// [`FETCH_CELL_MAX_BYTES`](crate::db::FETCH_CELL_MAX_BYTES) is deliberate —
/// it is already the app's answer to "how large a single value will we hold".
pub const MAX_FIELD_BYTES: usize = 8 * 1024 * 1024;

/// Largest single record, so a file with no line terminator at all (one line,
/// many small fields) is bounded too.
pub const MAX_RECORD_BYTES: usize = 32 * 1024 * 1024;

/// The CSV shape: what separates fields, what quotes them, and whether the
/// first record names the columns. Detected by [`sniff_dialect`] and freely
/// overridable — detection is a starting point, not a verdict.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CsvDialect {
    pub delimiter: u8,
    pub quote: u8,
    pub has_header: bool,
}

impl Default for CsvDialect {
    fn default() -> Self {
        CsvDialect {
            delimiter: b',',
            quote: b'"',
            has_header: true,
        }
    }
}

/// Pulls records one at a time out of a CSV byte stream.
pub struct CsvReader<R: BufRead> {
    input: R,
    delimiter: u8,
    quote: u8,
    encoding: Encoding,
    /// 1-based physical line the next byte belongs to.
    line: u64,
    /// Header names, once the header record has been read (`None` when the
    /// file is headerless — fields are then addressed by position).
    header: Option<Vec<String>>,
    primed: bool,
    finished: bool,
}

/// Parser position within a record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    /// Between fields: the next byte decides quoted vs. unquoted.
    FieldStart,
    Unquoted,
    Quoted,
    /// Just consumed the closing quote of a quoted field.
    QuoteClosed,
}

impl<R: BufRead> CsvReader<R> {
    /// Opens a reader over `input`. The header record, if the dialect says
    /// there is one, is read on the first [`Self::next_record`] call rather
    /// than here, so constructing a reader never fails.
    pub fn new(input: R, dialect: CsvDialect, encoding: Encoding) -> Self {
        CsvReader {
            input,
            delimiter: dialect.delimiter,
            quote: dialect.quote,
            encoding,
            line: 1,
            header: dialect.has_header.then(Vec::new),
            primed: false,
            finished: false,
        }
    }

    /// The header names, or `None` for a headerless file. Reads the header
    /// record if it has not been read yet.
    pub fn field_names(&mut self) -> Result<Option<Vec<String>>, ReadError> {
        self.prime()?;
        Ok(self.header.clone())
    }

    /// Reads the header record once, before the first data record.
    fn prime(&mut self) -> Result<(), ReadError> {
        if self.primed {
            return Ok(());
        }
        self.primed = true;
        self.strip_bom()?;
        if self.header.is_some() {
            // A file that holds nothing but a header (or nothing at all)
            // leaves the names empty; the mapping step then has nothing to
            // map, which is reported there rather than as a read failure.
            let names = self
                .read_record()?
                .map(|record| record.1)
                .unwrap_or_default();
            self.header = Some(names);
        }
        Ok(())
    }

    /// Consumes a UTF-8 byte-order mark, which Excel writes and which would
    /// otherwise become part of the first header name.
    fn strip_bom(&mut self) -> Result<(), ReadError> {
        let line = self.line;
        let buf = self.input.fill_buf().map_err(|e| io_read_error(line, e))?;
        if buf.starts_with(&[0xEF, 0xBB, 0xBF]) {
            self.input.consume(3);
        }
        Ok(())
    }

    fn next_byte(&mut self) -> Result<Option<u8>, ReadError> {
        let line = self.line;
        let buf = self.input.fill_buf().map_err(|e| io_read_error(line, e))?;
        match buf.first().copied() {
            Some(byte) => {
                self.input.consume(1);
                Ok(Some(byte))
            }
            None => Ok(None),
        }
    }

    /// Reads one record, returning the physical line it started on and its
    /// decoded fields. Blank lines are skipped; `None` means end of file.
    fn read_record(&mut self) -> Result<Option<(u64, Vec<String>)>, ReadError> {
        loop {
            if self.finished {
                return Ok(None);
            }
            let start_line = self.line;
            let mut fields: Vec<Vec<u8>> = Vec::new();
            let mut field: Vec<u8> = Vec::new();
            let mut state = State::FieldStart;
            // Whether anything made this a real record rather than a blank
            // line: a delimiter, a quote, or any field content. A CR is
            // NOT content — it is half of a CRLF terminator — so it must not
            // set this, or every blank line in a Windows file becomes a
            // record of one empty field.
            let mut structural = false;
            // Bytes held for this record, so a malformed file (see
            // [`MAX_FIELD_BYTES`]) is refused rather than accumulated.
            let mut record_bytes = 0usize;
            loop {
                let Some(byte) = self.next_byte()? else {
                    self.finished = true;
                    if state == State::Quoted {
                        return Err(unclosed_quote(start_line));
                    }
                    // A last line ending in a bare CR: still a terminator.
                    if field.last() == Some(&b'\r') {
                        field.pop();
                    }
                    fields.push(std::mem::take(&mut field));
                    break;
                };
                match state {
                    State::FieldStart | State::Unquoted => {
                        if byte == self.quote && state == State::FieldStart {
                            state = State::Quoted;
                            structural = true;
                        } else if byte == self.delimiter {
                            fields.push(std::mem::take(&mut field));
                            state = State::FieldStart;
                            structural = true;
                        } else if byte == b'\n' {
                            self.line += 1;
                            // CRLF: the CR belongs to the terminator.
                            if field.last() == Some(&b'\r') {
                                field.pop();
                            }
                            fields.push(std::mem::take(&mut field));
                            break;
                        } else {
                            field.push(byte);
                            record_bytes += 1;
                            state = State::Unquoted;
                            // A CR is provisionally part of a terminator: it
                            // is stripped again by the LF (or by end of file)
                            // above, and until then it is not evidence that
                            // this line holds anything.
                            structural = structural || byte != b'\r';
                        }
                    }
                    State::Quoted => {
                        if byte == self.quote {
                            state = State::QuoteClosed;
                        } else {
                            if byte == b'\n' {
                                self.line += 1;
                            }
                            field.push(byte);
                            record_bytes += 1;
                        }
                    }
                    State::QuoteClosed => {
                        if byte == self.quote {
                            // A doubled quote inside the quoted field.
                            field.push(self.quote);
                            state = State::Quoted;
                        } else if byte == self.delimiter {
                            fields.push(std::mem::take(&mut field));
                            state = State::FieldStart;
                        } else if byte == b'\n' {
                            self.line += 1;
                            fields.push(std::mem::take(&mut field));
                            break;
                        } else if byte == b'\r' {
                            // Waiting for the LF of a CRLF terminator.
                        } else {
                            // Text after the closing quote: keep it rather
                            // than reject the file.
                            field.push(byte);
                            record_bytes += 1;
                            state = State::Unquoted;
                            structural = true;
                        }
                    }
                }
                // Checked after every byte, so the bound holds however the
                // file is malformed — an unclosed quote is only the most
                // likely way to get here.
                if field.len() > MAX_FIELD_BYTES {
                    return Err(oversized(
                        start_line,
                        state == State::Quoted,
                        "field",
                        MAX_FIELD_BYTES,
                    ));
                }
                if record_bytes > MAX_RECORD_BYTES {
                    return Err(oversized(
                        start_line,
                        state == State::Quoted,
                        "record",
                        MAX_RECORD_BYTES,
                    ));
                }
            }
            // A line with no delimiter, no quote and no content is blank.
            if !structural && fields.len() == 1 && fields[0].is_empty() {
                continue;
            }
            let mut decoded = Vec::with_capacity(fields.len());
            for (index, bytes) in fields.iter().enumerate() {
                decoded.push(
                    decode_bytes(bytes, self.encoding).map_err(|message| ReadError {
                        line: Some(start_line),
                        message: format!("field {}: {message}", index + 1),
                    })?,
                );
            }
            return Ok(Some((start_line, decoded)));
        }
    }
}

impl<R: BufRead> RecordSource for CsvReader<R> {
    fn field_names(&mut self) -> Result<Option<Vec<String>>, ReadError> {
        CsvReader::field_names(self)
    }

    fn next_record(&mut self) -> Result<Option<Record>, ReadError> {
        self.prime()?;
        let Some((line, fields)) = self.read_record()? else {
            return Ok(None);
        };
        Ok(Some(Record::positional(line, fields)))
    }
}

/// Guesses the delimiter, quote character and header row from a sample of the
/// file (see [`SNIFF_BYTES`]).
///
/// The delimiter is chosen by *consistency*, not frequency: each candidate
/// parses the sample, and one that yields the same field count on every
/// record — more than one field — wins, highest field count first. A comma
/// that appears inside quoted prose therefore loses to the semicolon that
/// actually separates the fields, which counting alone would get backwards.
///
/// The trailing record of the sample is discarded before scoring: a cut-off
/// file ends mid-record, and that record's short field count would rule out
/// the correct delimiter.
pub fn sniff_dialect(sample: &[u8], encoding: Encoding) -> CsvDialect {
    let quote = sniff_quote(sample, encoding);
    let delimiter = sniff_delimiter(sample, quote, encoding);
    let has_header = sniff_header(sample, delimiter, quote, encoding);
    CsvDialect {
        delimiter,
        quote,
        has_header,
    }
}

/// Parses the sample with one candidate dialect, returning the records that
/// are certainly complete.
fn sample_records(sample: &[u8], delimiter: u8, quote: u8, encoding: Encoding) -> Vec<Vec<String>> {
    let dialect = CsvDialect {
        delimiter,
        quote,
        has_header: false,
    };
    let mut reader = CsvReader::new(sample, dialect, encoding);
    let mut records = Vec::new();
    while records.len() < SNIFF_RECORDS + 1 {
        match reader.read_record() {
            Ok(Some((_, fields))) => records.push(fields),
            // A parse failure (an unterminated quote in a truncated sample)
            // ends the sample rather than the detection.
            Ok(None) | Err(_) => break,
        }
    }
    // The sample may end mid-record, so its last record proves nothing.
    if records.len() > 1 && sample.len() >= SNIFF_BYTES {
        records.pop();
    }
    records
}

/// Picks the quote character: `"` unless the sample never uses one and `'`
/// appears where a quoted field can start (at the beginning of a line or
/// straight after a delimiter). An apostrophe inside a word never triggers
/// it, which is the whole point — `Fred's` must not turn the file into
/// single-quoted CSV.
fn sniff_quote(sample: &[u8], encoding: Encoding) -> u8 {
    if sample.contains(&b'"') {
        return b'"';
    }
    for delimiter in DELIMITERS {
        let mut at_field_start = true;
        for &byte in sample {
            if at_field_start && byte == b'\'' {
                // Only a real find if that quote also closes somewhere.
                let records = sample_records(sample, delimiter, b'\'', encoding);
                if records.len() > 1 {
                    return b'\'';
                }
                break;
            }
            at_field_start = byte == delimiter || byte == b'\n';
        }
    }
    b'"'
}

fn sniff_delimiter(sample: &[u8], quote: u8, encoding: Encoding) -> u8 {
    let mut best = (b',', 0usize);
    for candidate in DELIMITERS {
        let records = sample_records(sample, candidate, quote, encoding);
        let Some(first) = records.first() else {
            continue;
        };
        let width = first.len();
        if width < 2 || !records.iter().all(|record| record.len() == width) {
            continue;
        }
        if width > best.1 {
            best = (candidate, width);
        }
    }
    best.0
}

/// Guesses whether the first record names the columns: every field non-empty,
/// no duplicates, and none of them a bare number. A file whose first row is
/// data usually fails at least one of those.
fn sniff_header(sample: &[u8], delimiter: u8, quote: u8, encoding: Encoding) -> bool {
    let records = sample_records(sample, delimiter, quote, encoding);
    let Some(first) = records.first() else {
        return true;
    };
    let mut seen: Vec<String> = Vec::with_capacity(first.len());
    for field in first {
        let name = field.trim().to_ascii_lowercase();
        if name.is_empty() || seen.contains(&name) || field.trim().parse::<f64>().is_ok() {
            return false;
        }
        seen.push(name);
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read_all(text: &str, dialect: CsvDialect) -> Vec<(u64, Vec<String>)> {
        let bytes = text.as_bytes();
        let mut reader = CsvReader::new(bytes, dialect, Encoding::Utf8);
        reader.prime().unwrap();
        let mut out = Vec::new();
        while let Some(record) = reader.read_record().unwrap() {
            out.push(record);
        }
        out
    }

    fn headerless() -> CsvDialect {
        CsvDialect {
            has_header: false,
            ..CsvDialect::default()
        }
    }

    fn fields(text: &str) -> Vec<Vec<String>> {
        read_all(text, headerless())
            .into_iter()
            .map(|(_, f)| f)
            .collect()
    }

    #[test]
    fn plain_records_split_on_the_delimiter() {
        assert_eq!(
            fields("a,b,c\n1,2,3\n"),
            vec![
                vec!["a".to_string(), "b".into(), "c".into()],
                vec!["1".to_string(), "2".into(), "3".into()],
            ]
        );
    }

    #[test]
    fn quoted_fields_keep_delimiters_newlines_and_doubled_quotes() {
        let rows = fields("\"a,b\",\"line1\nline2\",\"say \"\"hi\"\"\"\n");
        assert_eq!(
            rows,
            vec![vec![
                "a,b".to_string(),
                "line1\nline2".into(),
                "say \"hi\"".into()
            ]]
        );
    }

    #[test]
    fn crlf_terminators_do_not_leak_into_the_last_field() {
        assert_eq!(
            fields("a,b\r\n1,2\r\n"),
            vec![
                vec!["a".to_string(), "b".into()],
                vec!["1".to_string(), "2".into()]
            ]
        );
    }

    #[test]
    fn blank_lines_produce_no_record() {
        // Including the newline that ends the final line, which must not
        // become an all-empty row.
        assert_eq!(
            fields("a,b\n\n\n1,2\n\n"),
            vec![
                vec!["a".to_string(), "b".into()],
                vec!["1".to_string(), "2".into()]
            ]
        );
    }

    #[test]
    fn crlf_blank_lines_produce_no_record_either() {
        // The Excel case, and the one this reader got wrong: a CR is half a
        // terminator, not content, so `\r\n\r\n` is a blank line. Treating it
        // as content put an all-empty record into every Windows export that
        // ends with a blank line — committed as an all-NULL row, or aborting
        // the whole import against a NOT NULL column at a line with no data.
        let expected = vec![
            vec!["a".to_string(), "b".into()],
            vec!["1".to_string(), "2".into()],
        ];
        assert_eq!(fields("a,b\r\n\r\n\r\n1,2\r\n\r\n"), expected);
        // Trailing blank line with and without its final terminator.
        assert_eq!(fields("a,b\r\n1,2\r\n\r\n"), expected);
        assert_eq!(fields("a,b\r\n1,2\r\n\r"), expected);
        // ...and the file that is nothing but blank lines holds no records.
        assert!(fields("\r\n\r\n").is_empty());
        assert!(fields("\r").is_empty());
    }

    #[test]
    fn a_line_of_only_delimiters_is_a_record_even_in_crlf() {
        // The other side of the same rule: a delimiter IS content, so this
        // must not be swept up as a blank line.
        assert_eq!(
            fields(",\r\n"),
            vec![vec!["".to_string(), "".into()]],
            "a delimiter makes a real record of empty fields"
        );
    }

    #[test]
    fn a_final_bare_cr_is_a_terminator_not_part_of_the_last_field() {
        assert_eq!(
            fields("a,b\r\n1,2\r"),
            vec![
                vec!["a".to_string(), "b".into()],
                vec!["1".to_string(), "2".into()]
            ]
        );
    }

    #[test]
    fn an_oversized_field_is_refused_instead_of_accumulated() {
        // The streaming guarantee has to hold for a *malformed* file too:
        // before this, an unclosed quote read the whole file into one field
        // and only reported the problem at EOF — 82 MB of memory to deliver
        // an error message about the first line.
        let mut oversized = String::from("id,name\n1,\"");
        oversized.push_str(&"x".repeat(MAX_FIELD_BYTES + 1024));
        let mut reader =
            CsvReader::new(oversized.as_bytes(), CsvDialect::default(), Encoding::Utf8);
        let err = reader.next_record().unwrap_err();
        assert_eq!(err.line, Some(2));
        assert!(err.message.contains("larger than 8 MiB"), "{}", err.message);
        // The message points at the likely cause rather than just the size.
        assert!(err.message.contains("quote character"), "{}", err.message);
    }

    #[test]
    fn an_oversized_record_without_any_newline_is_refused_too() {
        // No unclosed quote here: just a file with no line terminator, which
        // the field bound alone would not catch.
        let field_size = MAX_FIELD_BYTES / 8;
        let one_field = "x".repeat(field_size);
        let mut oversized = String::new();
        for _ in 0..(MAX_RECORD_BYTES / field_size + 2) {
            oversized.push_str(&one_field);
            oversized.push(',');
        }
        let mut reader = CsvReader::new(oversized.as_bytes(), headerless(), Encoding::Utf8);
        let err = reader.next_record().unwrap_err();
        assert!(
            err.message.contains("larger than 32 MiB"),
            "{}",
            err.message
        );
        assert!(err.message.contains("delimiter"), "{}", err.message);
    }

    #[test]
    fn an_empty_field_is_a_record_of_its_own_kind() {
        // A line of nothing but delimiters is structural, not blank.
        assert_eq!(
            fields(",,\n"),
            vec![vec!["".to_string(), "".into(), "".into()]]
        );
    }

    #[test]
    fn records_report_the_physical_line_they_started_on() {
        // The embedded newline inside the quoted field means record 3 starts
        // on line 4 — which is what a skip report has to name.
        let rows = read_all("a\n\"b\nc\"\nd\n", headerless());
        let lines: Vec<u64> = rows.iter().map(|(line, _)| *line).collect();
        assert_eq!(lines, vec![1, 2, 4]);
    }

    #[test]
    fn a_header_is_consumed_and_reported_as_field_names() {
        let bytes = b"id,name\n1,ada\n".as_slice();
        let mut reader = CsvReader::new(bytes, CsvDialect::default(), Encoding::Utf8);
        assert_eq!(
            reader.field_names().unwrap(),
            Some(vec!["id".to_string(), "name".to_string()])
        );
        let record = reader.next_record().unwrap().unwrap();
        assert_eq!(record.line, 2);
        assert_eq!(reader.next_record().unwrap(), None);
    }

    #[test]
    fn an_unterminated_quote_is_reported_with_its_line() {
        let bytes = b"a,b\n1,\"oops\n".as_slice();
        let mut reader = CsvReader::new(bytes, headerless(), Encoding::Utf8);
        reader.prime().unwrap();
        reader.read_record().unwrap();
        let err = reader.read_record().unwrap_err();
        assert_eq!(err.line, Some(2));
        assert!(err.message.contains("never closed"), "{}", err.message);
    }

    #[test]
    fn text_after_a_closing_quote_is_kept_rather_than_rejected() {
        assert_eq!(
            fields("\"a\"b,c\n"),
            vec![vec!["ab".to_string(), "c".into()]]
        );
    }

    #[test]
    fn a_utf8_bom_is_not_part_of_the_first_field() {
        let mut bytes = vec![0xEF, 0xBB, 0xBF];
        bytes.extend_from_slice(b"id,name\n");
        let mut reader = CsvReader::new(bytes.as_slice(), CsvDialect::default(), Encoding::Utf8);
        assert_eq!(
            reader.field_names().unwrap(),
            Some(vec!["id".to_string(), "name".to_string()])
        );
    }

    #[test]
    fn latin1_decodes_where_utf8_would_fail() {
        // 0xE9 is 'é' in Latin-1 and an invalid UTF-8 lead byte.
        let bytes = [b'a', b',', 0xE9, b'\n'];
        let rows: Vec<Vec<String>> = {
            let mut reader = CsvReader::new(bytes.as_slice(), headerless(), Encoding::Latin1);
            let mut out = Vec::new();
            while let Some(record) = reader.next_record().unwrap() {
                out.push(
                    record
                        .present_fields()
                        .iter()
                        .map(|field| record.field(field).display())
                        .collect(),
                );
            }
            out
        };
        assert_eq!(rows, vec![vec!["a".to_string(), "é".into()]]);

        let mut utf8 = CsvReader::new(bytes.as_slice(), headerless(), Encoding::Utf8);
        let err = utf8.next_record().unwrap_err();
        assert_eq!(err.line, Some(1));
        assert!(err.message.contains("UTF-8"), "{}", err.message);
    }

    #[test]
    fn sniffing_prefers_the_delimiter_that_gives_a_consistent_shape() {
        // Commas appear more often than semicolons — inside quoted prose —
        // so frequency alone would pick the wrong one.
        let sample = b"a;b;c\n\"x,y,z\";2;3\n\"p,q,r\";5;6\n";
        let dialect = sniff_dialect(sample, Encoding::Utf8);
        assert_eq!(dialect.delimiter, b';');
        assert_eq!(dialect.quote, b'"');
        assert!(dialect.has_header);
    }

    #[test]
    fn sniffing_finds_tabs_and_pipes() {
        for (sample, expected) in [
            ("id\tname\n1\tada\n", b'\t'),
            ("id|name\n1|ada\n", b'|'),
            ("id,name\n1,ada\n", b','),
        ] {
            assert_eq!(
                sniff_dialect(sample.as_bytes(), Encoding::Utf8).delimiter,
                expected,
                "{sample:?}"
            );
        }
    }

    #[test]
    fn sniffing_takes_single_quotes_only_where_a_field_starts() {
        // An apostrophe inside a word is not a quote character.
        let apostrophes = b"id,name\n1,Fred's\n2,Ada's\n";
        assert_eq!(sniff_dialect(apostrophes, Encoding::Utf8).quote, b'"');

        let single = b"id,name\n1,'Ada, A'\n2,'Fred'\n";
        let dialect = sniff_dialect(single, Encoding::Utf8);
        assert_eq!(dialect.quote, b'\'');
        assert_eq!(dialect.delimiter, b',');
    }

    #[test]
    fn header_detection_rejects_a_first_row_of_data() {
        assert!(!sniff_dialect(b"1,2,3\n4,5,6\n", Encoding::Utf8).has_header);
        assert!(!sniff_dialect(b"a,a\nx,y\n", Encoding::Utf8).has_header);
        assert!(!sniff_dialect(b"a,,c\nx,y,z\n", Encoding::Utf8).has_header);
        assert!(sniff_dialect(b"a,b,c\n1,2,3\n", Encoding::Utf8).has_header);
    }
}
