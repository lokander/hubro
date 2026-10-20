//! The byte-at-a-time reading both import readers are built on: a
//! [`BufRead`] with the physical line counted, a UTF-8 byte-order mark
//! dropped, and I/O failures reported with the line they happened on.
//!
//! The CSV and JSON readers used to carry a copy each, which is how the JSON
//! reader went without BOM stripping long after the CSV one had it. One
//! cursor means a fix to how a file is *read*, as opposed to how it is
//! *parsed*, lands in both.

use std::io::{self, BufRead};

use super::ReadError;

/// A pull-based byte reader over `R`, counting lines as it goes.
pub(super) struct ByteCursor<R: BufRead> {
    input: R,
    /// 1-based physical line the next byte belongs to.
    line: u64,
    /// Whether the start of the input has been checked for a byte-order mark.
    bom_checked: bool,
}

impl<R: BufRead> ByteCursor<R> {
    pub(super) fn new(input: R) -> Self {
        ByteCursor {
            input,
            line: 1,
            bom_checked: false,
        }
    }

    /// The 1-based physical line the next byte belongs to.
    pub(super) fn line(&self) -> u64 {
        self.line
    }

    /// The next byte, without consuming it; `None` at end of input.
    ///
    /// The first call drops a UTF-8 byte-order mark, which Excel and
    /// PowerShell's `Out-File` write and which would otherwise become part of
    /// the first header name, or make a JSON file "start with `ï`".
    pub(super) fn peek(&mut self) -> Result<Option<u8>, ReadError> {
        let line = self.line;
        let buf = self.input.fill_buf().map_err(|e| io_read_error(line, e))?;
        if !self.bom_checked {
            self.bom_checked = true;
            if buf.starts_with(&[0xEF, 0xBB, 0xBF]) {
                self.input.consume(3);
                return self.peek();
            }
        }
        Ok(buf.first().copied())
    }

    /// Consumes `byte`, the one [`Self::peek`] just returned.
    pub(super) fn bump(&mut self, byte: u8) {
        if byte == b'\n' {
            self.line += 1;
        }
        self.input.consume(1);
    }

    /// Consumes and returns the next byte; `None` at end of input.
    pub(super) fn next_byte(&mut self) -> Result<Option<u8>, ReadError> {
        let next = self.peek()?;
        if let Some(byte) = next {
            self.bump(byte);
        }
        Ok(next)
    }
}

/// Wraps an I/O failure while reading the file, naming the line reached.
fn io_read_error(line: u64, err: io::Error) -> ReadError {
    ReadError {
        line: Some(line),
        message: format!("reading the file failed: {err}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn drain(bytes: &[u8]) -> (Vec<u8>, u64) {
        let mut cursor = ByteCursor::new(bytes);
        let mut out = Vec::new();
        while let Some(byte) = cursor.next_byte().unwrap() {
            out.push(byte);
        }
        (out, cursor.line())
    }

    #[test]
    fn a_leading_bom_is_dropped_and_nothing_else_is() {
        let mut bytes = vec![0xEF, 0xBB, 0xBF];
        bytes.extend_from_slice(b"a\nb");
        assert_eq!(drain(&bytes), (b"a\nb".to_vec(), 2));
        // Only at the start: the same bytes later on are content.
        let mut later = b"a".to_vec();
        later.extend_from_slice(&[0xEF, 0xBB, 0xBF]);
        assert_eq!(drain(&later).0, later);
    }

    #[test]
    fn every_consumed_newline_advances_the_line() {
        assert_eq!(drain(b"\n\r\nx\n").1, 4);
        let mut cursor = ByteCursor::new(b"\n".as_slice());
        // Peeking is not consuming.
        assert_eq!(cursor.peek().unwrap(), Some(b'\n'));
        assert_eq!(cursor.line(), 1);
    }
}
