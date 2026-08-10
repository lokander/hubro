//! Streaming JSON reading for imports (FRE-112): a top-level array of
//! objects, and newline-delimited JSON.
//!
//! Both shapes are read by the same scanner, because they differ only in what
//! separates two records — a comma inside `[...]`, whitespace for NDJSON.
//! The scanner finds the extent of ONE value by tracking brace/bracket depth
//! and string state, then hands that slice to `serde_json`. That is what
//! makes the read streaming: `serde_json::from_reader` on the whole file
//! would materialize every record at once, and the export it mirrors streams
//! one row at a time.
//!
//! Records are objects keyed by field name; a top-level value that is not an
//! object is reported with its line, since there is nothing to map its
//! columns from.

use std::io::{self, BufRead};

use super::{decode_bytes, Encoding, ReadError, Record, RecordSource};

/// Wraps an I/O failure while reading the file, naming the line reached.
fn io_read_error(line: u64, err: io::Error) -> ReadError {
    ReadError {
        line: Some(line),
        message: format!("reading the file failed: {err}"),
    }
}

/// Which JSON layout a file uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum JsonShape {
    /// One top-level array of objects — what a "download as JSON" button
    /// emits, and what hubro's own export writes.
    #[default]
    Array,
    /// One JSON value per line (NDJSON / JSON Lines) — what most tools emit
    /// for large data, since it needs no closing bracket.
    Lines,
}

/// Guesses the layout from the start of a file: a leading `[` means an array,
/// anything else is treated as newline-delimited.
pub fn sniff_shape(sample: &[u8]) -> JsonShape {
    match sample.iter().find(|b| !b.is_ascii_whitespace()) {
        Some(b'[') => JsonShape::Array,
        _ => JsonShape::Lines,
    }
}

/// Pulls JSON records one at a time out of a byte stream.
pub struct JsonReader<R: BufRead> {
    input: R,
    shape: JsonShape,
    encoding: Encoding,
    /// 1-based physical line the next byte belongs to.
    line: u64,
    /// Array mode: whether the opening `[` has been consumed.
    opened: bool,
    finished: bool,
}

impl<R: BufRead> JsonReader<R> {
    pub fn new(input: R, shape: JsonShape, encoding: Encoding) -> Self {
        JsonReader {
            input,
            shape,
            encoding,
            line: 1,
            opened: false,
            finished: false,
        }
    }

    fn peek(&mut self) -> Result<Option<u8>, ReadError> {
        let line = self.line;
        let buf = self.input.fill_buf().map_err(|e| io_read_error(line, e))?;
        Ok(buf.first().copied())
    }

    fn bump(&mut self, byte: u8) {
        if byte == b'\n' {
            self.line += 1;
        }
        self.input.consume(1);
    }

    /// Consumes whitespace, returning the next significant byte without
    /// consuming it.
    fn skip_whitespace(&mut self) -> Result<Option<u8>, ReadError> {
        loop {
            let Some(byte) = self.peek()? else {
                return Ok(None);
            };
            if byte.is_ascii_whitespace() {
                self.bump(byte);
            } else {
                return Ok(Some(byte));
            }
        }
    }

    /// Consumes and returns the bytes of exactly one JSON value.
    ///
    /// Composite values are scanned by depth, with strings (and their
    /// escapes) skipped so a `}` inside a string cannot end the record early.
    /// Scalars run to the first byte that cannot continue one.
    fn scan_value(&mut self) -> Result<Vec<u8>, ReadError> {
        let mut raw = Vec::new();
        let mut depth = 0usize;
        let mut in_string = false;
        let mut escaped = false;
        loop {
            let Some(byte) = self.peek()? else {
                if depth > 0 || in_string {
                    return Err(ReadError {
                        line: Some(self.line),
                        message: "the file ends in the middle of a JSON value".to_string(),
                    });
                }
                break;
            };
            if in_string {
                raw.push(byte);
                self.bump(byte);
                if escaped {
                    escaped = false;
                } else if byte == b'\\' {
                    escaped = true;
                } else if byte == b'"' {
                    in_string = false;
                    if depth == 0 {
                        break;
                    }
                }
                continue;
            }
            match byte {
                b'"' => {
                    in_string = true;
                    raw.push(byte);
                    self.bump(byte);
                }
                b'{' | b'[' => {
                    depth += 1;
                    raw.push(byte);
                    self.bump(byte);
                }
                b'}' | b']' => {
                    // A closer at depth 0 belongs to the enclosing array, not
                    // to this value.
                    if depth == 0 {
                        break;
                    }
                    depth -= 1;
                    raw.push(byte);
                    self.bump(byte);
                    if depth == 0 {
                        break;
                    }
                }
                b',' if depth == 0 => break,
                _ => {
                    if depth == 0 && byte.is_ascii_whitespace() {
                        break;
                    }
                    raw.push(byte);
                    self.bump(byte);
                }
            }
        }
        Ok(raw)
    }

    /// Reads the next record, or `None` at the end of the data.
    fn read_record(&mut self) -> Result<Option<Record>, ReadError> {
        if self.finished {
            return Ok(None);
        }
        if self.shape == JsonShape::Array && !self.opened {
            match self.skip_whitespace()? {
                Some(b'[') => {
                    self.bump(b'[');
                    self.opened = true;
                }
                Some(other) => {
                    return Err(ReadError {
                        line: Some(self.line),
                        message: format!(
                            "expected a JSON array, but the file starts with {:?}",
                            other as char
                        ),
                    })
                }
                None => {
                    self.finished = true;
                    return Ok(None);
                }
            }
        }
        // Separators between records: commas in an array, whitespace in
        // NDJSON (already skipped).
        loop {
            match self.skip_whitespace()? {
                None => {
                    self.finished = true;
                    return Ok(None);
                }
                Some(b',') if self.shape == JsonShape::Array => self.bump(b','),
                Some(b']') if self.shape == JsonShape::Array => {
                    self.bump(b']');
                    self.finished = true;
                    return Ok(None);
                }
                Some(_) => break,
            }
        }
        let start_line = self.line;
        let raw = self.scan_value()?;
        if raw.is_empty() {
            self.finished = true;
            return Ok(None);
        }
        let text = decode_bytes(&raw, self.encoding).map_err(|message| ReadError {
            line: Some(start_line),
            message,
        })?;
        let value: serde_json::Value = serde_json::from_str(&text).map_err(|err| ReadError {
            line: Some(start_line),
            message: format!("invalid JSON: {err}"),
        })?;
        match value {
            serde_json::Value::Object(map) => Ok(Some(Record::keyed(start_line, map))),
            other => Err(ReadError {
                line: Some(start_line),
                message: format!(
                    "expected an object with one field per column, found {}",
                    describe(&other)
                ),
            }),
        }
    }
}

/// Names a JSON value's kind for an error message.
fn describe(value: &serde_json::Value) -> &'static str {
    match value {
        serde_json::Value::Null => "null",
        serde_json::Value::Bool(_) => "a boolean",
        serde_json::Value::Number(_) => "a number",
        serde_json::Value::String(_) => "a string",
        serde_json::Value::Array(_) => "an array",
        serde_json::Value::Object(_) => "an object",
    }
}

impl<R: BufRead> RecordSource for JsonReader<R> {
    /// JSON records carry their own field names, so there is no separate
    /// header to report — the mapping step reads the names off the first
    /// records instead.
    fn field_names(&mut self) -> Result<Option<Vec<String>>, ReadError> {
        Ok(None)
    }

    fn next_record(&mut self) -> Result<Option<Record>, ReadError> {
        self.read_record()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::import::SourceValue;

    fn records(text: &str, shape: JsonShape) -> Vec<Record> {
        let mut reader = JsonReader::new(text.as_bytes(), shape, Encoding::Utf8);
        let mut out = Vec::new();
        while let Some(record) = reader.next_record().unwrap() {
            out.push(record);
        }
        out
    }

    fn text_of(record: &Record, key: &str) -> String {
        match record.field(&crate::db::import::SourceField::Key(key.to_string())) {
            SourceValue::Json(value) => value.to_string(),
            SourceValue::Text(text) => text,
            SourceValue::Missing => "<missing>".to_string(),
        }
    }

    #[test]
    fn an_array_of_objects_streams_element_by_element() {
        let rows = records(
            r#"[{"id":1,"name":"ada"},{"id":2,"name":"grace"}]"#,
            JsonShape::Array,
        );
        assert_eq!(rows.len(), 2);
        assert_eq!(text_of(&rows[0], "name"), "\"ada\"");
        assert_eq!(text_of(&rows[1], "id"), "2");
    }

    #[test]
    fn an_empty_array_holds_no_records() {
        assert!(records("[]", JsonShape::Array).is_empty());
        assert!(records("  [\n]\n", JsonShape::Array).is_empty());
    }

    #[test]
    fn newline_delimited_json_reads_one_object_per_line() {
        let rows = records("{\"id\":1}\n{\"id\":2}\n{\"id\":3}\n", JsonShape::Lines);
        assert_eq!(rows.len(), 3);
        assert_eq!(rows.iter().map(|r| r.line).collect::<Vec<_>>(), [1, 2, 3]);
    }

    #[test]
    fn nested_values_and_braces_inside_strings_do_not_end_a_record_early() {
        // The `}` and `,` inside the string, and the nested object, all have
        // to be scanned through — getting either wrong splits one record into
        // two.
        let rows = records(
            r#"[{"a":{"b":[1,2]},"s":"} , ] \" {"},{"a":null,"s":"x"}]"#,
            JsonShape::Array,
        );
        assert_eq!(rows.len(), 2);
        assert_eq!(text_of(&rows[0], "a"), r#"{"b":[1,2]}"#);
        assert_eq!(text_of(&rows[0], "s"), r#""} , ] \" {""#);
        assert_eq!(text_of(&rows[1], "a"), "null");
    }

    #[test]
    fn records_report_the_line_they_start_on() {
        let rows = records("[\n  {\"id\":1},\n  {\"id\":2}\n]\n", JsonShape::Array);
        assert_eq!(rows.iter().map(|r| r.line).collect::<Vec<_>>(), [2, 3]);
    }

    #[test]
    fn a_non_object_record_is_refused_by_line() {
        let mut reader = JsonReader::new(
            "[{\"id\":1},\n 7]".as_bytes(),
            JsonShape::Array,
            Encoding::Utf8,
        );
        reader.next_record().unwrap();
        let err = reader.next_record().unwrap_err();
        assert_eq!(err.line, Some(2));
        assert!(err.message.contains("a number"), "{}", err.message);
    }

    #[test]
    fn a_truncated_value_is_an_error_not_a_silent_stop() {
        let mut reader = JsonReader::new("[{\"id\":1".as_bytes(), JsonShape::Array, Encoding::Utf8);
        let err = reader.next_record().unwrap_err();
        assert!(
            err.message.contains("ends in the middle"),
            "{}",
            err.message
        );
    }

    #[test]
    fn a_file_that_is_not_an_array_is_refused_in_array_mode() {
        let mut reader = JsonReader::new("{\"id\":1}".as_bytes(), JsonShape::Array, Encoding::Utf8);
        let err = reader.next_record().unwrap_err();
        assert!(
            err.message.contains("expected a JSON array"),
            "{}",
            err.message
        );
    }

    #[test]
    fn the_shape_sniffer_reads_the_first_significant_byte() {
        assert_eq!(sniff_shape(b"  \n [ {} ]"), JsonShape::Array);
        assert_eq!(sniff_shape(b"{\"id\":1}\n"), JsonShape::Lines);
        assert_eq!(sniff_shape(b""), JsonShape::Lines);
    }
}
