//! Multi-statement SQL scripts: splitting, classification, and execution.
//!
//! [`split_statements`] is a lexer-level splitter — it understands quotes,
//! comments, dollar-quoting, and routine and trigger bodies well enough to
//! find statement boundaries, without parsing SQL. [`run_script`] executes
//! the statements one by one, stopping at the first error. A multi-statement
//! script runs **atomically** in one transaction ([`wrap_atomically`]) — a
//! mid-script failure rolls the whole thing back — unless it manages
//! transactions itself (`BEGIN`/`COMMIT`) or contains a statement that can't
//! run inside a transaction (e.g. `VACUUM`, Postgres `CREATE INDEX
//! CONCURRENTLY`), in which case it falls back to sequential autocommit and
//! earlier statements' effects persist. Either way the whole script runs on
//! one connection, and a transaction the script opens itself and leaves open
//! is rolled back when the run ends.

use std::ops::Range;

use super::caps::{self, Capabilities};
use super::error::DbError;
use super::registry::{
    engine_enforces_read_only, DbPool, ReadOnlyRun, RolledBack, ScriptConn, ScriptTx,
    MAX_QUERY_ROWS, PLAN_CELL_CAP, QUERY_CELL_CAP,
};
use super::sql::Dialect;
use super::value::QueryResult;
use super::value::{Capped, PendingRest, RestOfRead};

/// One lexed region of a SQL script. [`lex_regions`] is the single place
/// that understands quote/comment/bracket/dollar-quote state — the statement
/// splitter, the string/comment stripper, the first-keyword scanner and the
/// DDL terminator ([`super::ddl::terminate`]) are all thin consumers of it,
/// so a lexing edge case fixed here is fixed in all of them at once.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RegionKind {
    /// Plain SQL: keywords, identifiers, operators, whitespace.
    Code,
    /// A quoted string or identifier, quotes included: `'…'` and `"…"`
    /// everywhere, `` `…` `` on SQLite, and a Postgres escape string
    /// (`E'…'`, whose backslashes escape). Doubled quotes need no special
    /// casing: `'it''s'` lexes as two adjacent String regions, which every
    /// consumer treats the same as one.
    String,
    /// A line comment (`-- …`, excluding the line end — the splitter's GO
    /// line tracking must see that newline as code) or a block comment
    /// (`/* … */`). Where each ends is the engine's own rule: see
    /// [`region_at`].
    Comment,
    /// A `[bracketed identifier]`, brackets included, so `[a;b]` or `[a--b]`
    /// never split or start comments. SQL Server (where `]]` is an escaped
    /// `]`) and SQLite (no escape). Not Postgres, where `[` subscripts an
    /// array. SQLite used to be left out as a compat quirk nobody would
    /// write `;` inside, but a quote in one (`[']`) then opened a "string"
    /// the gate could not see past while SQLite ran everything after it.
    Bracket,
    /// A dollar-quoted string (`$$…$$`, `$tag$…$tag$`) — Postgres only,
    /// delimiters included. A bare `$` (e.g. a `$1` parameter placeholder)
    /// is code, not a delimiter.
    DollarQuote,
}

/// Lexes `sql` into contiguous regions covering every byte, in order.
///
/// Unterminated strings, comments, brackets, and dollar quotes swallow the
/// rest of the input as their region — the graceful degradation every
/// consumer wants. Known limitation: a Postgres session with
/// `standard_conforming_strings` off reads backslashes in every string;
/// this lexer assumes the default (on), honouring them only in `E'…'`.
fn lex_regions(
    sql: &str,
    dialect: Dialect,
) -> impl Iterator<Item = (Range<usize>, RegionKind)> + '_ {
    let bytes = sql.as_bytes();
    let mut i = 0usize;
    std::iter::from_fn(move || {
        if i >= bytes.len() {
            return None;
        }
        let start = i;
        if let Some((end, kind)) = region_at(sql, i, dialect) {
            i = end;
            return Some((start..end, kind));
        }
        // A code byte: extend until the next non-code region opens (or the
        // input ends), so code comes out as maximal runs.
        i += 1;
        while i < bytes.len() && region_at(sql, i, dialect).is_none() {
            i += 1;
        }
        Some((start..i, RegionKind::Code))
    })
}

/// If a non-code region opens at byte `i` of `sql`, returns its end offset
/// (one past the closing delimiter, or end of input when unterminated) and
/// its kind.
///
/// **Every boundary here must be the engine's own, in both directions.** The
/// capability gate reads the stripped code this lexer leaves, and the
/// splitter's statements are what run: text the lexer calls a string or
/// comment while the engine calls it code is a statement the gate never
/// sees, and text the lexer calls code while the engine calls it a string
/// can open a "string" that hides the real code after it. Each rule below is
/// measured against SQLite 3.53, PostgreSQL 17 and SQL Server 2022.
fn region_at(sql: &str, i: usize, dialect: Dialect) -> Option<(usize, RegionKind)> {
    let bytes = sql.as_bytes();
    let previous = i.checked_sub(1).map(|p| bytes[p]);
    match bytes[i] {
        b'\'' if dialect == Dialect::Postgres && opens_escape_string(bytes, i) => {
            Some((escape_string_end(bytes, i), RegionKind::String))
        }
        // `'…'` strings everywhere; `"…"` identifiers everywhere; `` `…` ``
        // identifiers on SQLite only (Postgres and SQL Server reject the
        // character). A doubled quote lexes as two adjacent regions.
        quote @ (b'\'' | b'"') => Some((quoted_end(bytes, i, quote), RegionKind::String)),
        b'`' if dialect == Dialect::Sqlite => {
            Some((quoted_end(bytes, i, b'`'), RegionKind::String))
        }
        // `[…]` identifiers: SQL Server escapes `]` as `]]`; SQLite has no
        // escape at all (measured: `[a]]b]` is "unrecognized token: ]").
        // Postgres uses `[` for array subscripts, so it is code there.
        b'[' if dialect != Dialect::Postgres => {
            let mut j = i + 1;
            while j < bytes.len() {
                if bytes[j] == b']' {
                    if dialect == Dialect::SqlServer && bytes.get(j + 1) == Some(&b']') {
                        j += 2; // escaped ]] stays inside
                    } else {
                        j += 1; // past the closing bracket
                        break;
                    }
                } else {
                    j += 1;
                }
            }
            Some((j, RegionKind::Bracket))
        }
        // A line comment ends at `\n` on SQLite, and at `\n` or a bare `\r`
        // on Postgres and SQL Server (measured: `--c<CR>, 2 AS b` returns two
        // columns on both, one on SQLite).
        b'-' if bytes.get(i + 1) == Some(&b'-') => {
            let ends_line = |b: u8| b == b'\n' || (b == b'\r' && dialect != Dialect::Sqlite);
            let mut j = i + 2;
            while j < bytes.len() && !ends_line(bytes[j]) {
                j += 1;
            }
            Some((j, RegionKind::Comment))
        }
        // Block comments nest on Postgres and SQL Server and not on SQLite
        // (measured: SQLite ends `/* /* */` at the first `*/`).
        b'/' if bytes.get(i + 1) == Some(&b'*') => {
            Some((block_comment_end(bytes, i, dialect).0, RegionKind::Comment))
        }
        // Dollar quotes are Postgres only, and never straight after an
        // identifier byte: Postgres reads `x$y$` as one identifier, as SQLite
        // and SQL Server read any `$` inside a name. (`$1$q$` is a parameter
        // then a quote there, but a script binds no parameters, so a
        // statement holding `$1` cannot run at all.)
        b'$' if dialect == Dialect::Postgres && !previous.is_some_and(is_word_byte) => {
            let open_end = dollar_tag_end(bytes, i)?;
            let delimiter = &sql[i..open_end];
            let end = match sql[open_end..].find(delimiter) {
                Some(close) => open_end + close + delimiter.len(),
                None => sql.len(), // unterminated: rest is the string
            };
            Some((end, RegionKind::DollarQuote))
        }
        _ => None, // a code byte
    }
}

/// One past the end of the block comment opening at `bytes[at]` (or the end
/// of input when it never closes), and how many levels are still open there:
/// 0 for a closed comment, more than 1 only where comments nest.
fn block_comment_end(bytes: &[u8], at: usize, dialect: Dialect) -> (usize, usize) {
    let nests = dialect != Dialect::Sqlite;
    let mut depth = 1usize;
    let mut j = at + 2;
    while j < bytes.len() && depth > 0 {
        if nests && bytes[j] == b'/' && bytes.get(j + 1) == Some(&b'*') {
            depth += 1;
            j += 2;
        } else if bytes[j] == b'*' && bytes.get(j + 1) == Some(&b'/') {
            depth -= 1;
            j += 2;
        } else {
            j += 1;
        }
    }
    (j, depth)
}

/// One past the closing `quote` of the region opening at `bytes[at]`, or the
/// end of input when it never closes.
fn quoted_end(bytes: &[u8], at: usize, quote: u8) -> usize {
    let mut j = at + 1;
    while j < bytes.len() && bytes[j] != quote {
        j += 1;
    }
    (j + 1).min(bytes.len())
}

/// Whether the `'` at `bytes[at]` opens a Postgres escape string: it follows
/// an `E`/`e` that starts a token (one after an identifier byte, as in
/// `nameE'…'`, is the end of that name, and the string is a plain one).
fn opens_escape_string(bytes: &[u8], at: usize) -> bool {
    at >= 1 && matches!(bytes[at - 1], b'E' | b'e') && !(at >= 2 && is_word_byte(bytes[at - 2]))
}

/// One past the end of a Postgres escape string opening at `bytes[at]`,
/// including any continuation segments.
///
/// Inside, `\` escapes the next byte and `''` is a quote, so `E'\''` is one
/// string holding a quote — a plain-string reading would close it at the
/// second quote and open a new "string" at the third, hiding the code after
/// it. A segment followed by whitespace holding a newline (comments
/// allowed) and another `'` continues the same string, still with escapes.
fn escape_string_end(bytes: &[u8], at: usize) -> usize {
    let mut j = at + 1;
    loop {
        while j < bytes.len() {
            match bytes[j] {
                b'\\' => j += 2,
                b'\'' if bytes.get(j + 1) == Some(&b'\'') => j += 2,
                b'\'' => break,
                _ => j += 1,
            }
        }
        if j >= bytes.len() {
            return bytes.len();
        }
        j += 1; // past the closing quote
        match continuation_quote(bytes, j) {
            Some(next) => j = next + 1,
            None => return j,
        }
    }
}

/// If Postgres continues a string ending just before `bytes[from]` — only
/// whitespace and `--` comments follow, with at least one newline, then a
/// `'` — returns the index of that `'`.
fn continuation_quote(bytes: &[u8], from: usize) -> Option<usize> {
    let mut j = from;
    let mut newline = false;
    loop {
        match bytes.get(j)? {
            b'\n' | b'\r' => {
                newline = true;
                j += 1;
            }
            b' ' | b'\t' | b'\x0b' | b'\x0c' => j += 1,
            b'-' if newline && bytes.get(j + 1) == Some(&b'-') => {
                while j < bytes.len() && !matches!(bytes[j], b'\n' | b'\r') {
                    j += 1;
                }
            }
            b'\'' if newline => return Some(j),
            _ => return None,
        }
    }
}

/// Splits a script into the units a run sends: on SQL Server its GO batches,
/// elsewhere its statements, split on `;` ([`split_into`] says how).
///
/// **A SQL Server batch is sent whole**, as SSMS and sqlcmd send it: a
/// variable lives exactly as long as the batch that declares it, so
/// `DECLARE @x int = 1; INSERT INTO t VALUES (@x)` split at its `;` failed
/// the `INSERT` with error 137 (measured), and runs as one batch. What hubro
/// decides from a unit's text — what the gate charges, whether the script is
/// wrapped, how a batch is sent — it decides over the batch's statements
/// ([`split_tsql_statements`]), each as it decided before.
pub fn split_statements(sql: &str, dialect: Dialect) -> Vec<String> {
    split_into(sql, dialect, dialect != Dialect::SqlServer)
}

/// A SQL Server batch's statements, split on `;` as well as `GO` — what each
/// per-statement decision over a batch is folded from (see
/// [`split_statements`]). A single statement splits into itself.
pub fn split_tsql_statements(sql: &str) -> Vec<String> {
    split_into(sql, Dialect::SqlServer, true)
}

/// A SQL Server batch's statements when it holds more than one, for a
/// decision to fold over; `None` for one statement, and on every other
/// dialect, whose units are single statements already.
fn batch_statements(sql: &str, dialect: Dialect) -> Option<Vec<String>> {
    if dialect != Dialect::SqlServer {
        return None;
    }
    let statements = split_tsql_statements(sql);
    (statements.len() > 1).then_some(statements)
}

/// Whether a unit returns rows — a SQL Server batch when any of its
/// statements does, so a batch that declares a variable and then reads it is
/// read, not run for its counts.
fn returns_rows(sql: &str, dialect: Dialect) -> bool {
    match batch_statements(sql, dialect) {
        Some(statements) => statements
            .iter()
            .any(|s| classify_statement(s, dialect).returns_rows()),
        None => classify_statement(sql, dialect).returns_rows(),
    }
}

/// Splits a script on `GO` (SQL Server) and, when `semicolons`, on `;`,
/// respecting quoted strings and identifiers, comments, and dollar quotes,
/// each by the dialect's own rules — see [`region_at`]. A trailing statement
/// without a semicolon is kept; statements that are empty (only whitespace
/// and/or comments) are skipped.
///
/// On SQL Server, `GO` separates batches. `GO` is a client-side
/// batch separator (SSMS/sqlcmd), not T-SQL: it only counts when it stands
/// alone on its own line (leading whitespace allowed), optionally followed
/// by a repeat count — `GO 5` is treated as a plain separator with the
/// count ignored (repeating a batch is a scripting-tool feature, not
/// something a viewer should replay). `GO` inside strings, comments, or
/// bracketed identifiers, or sharing a line with other code, never splits.
///
/// A `;` inside a routine or trigger body does not split either — see
/// [`BodyTracker`] for what counts as a body on each dialect.
///
/// On SQL Server a statement keeps the `;` it was split on, and only there:
/// T-SQL requires one after a `MERGE` (error 10713 without it). The server
/// takes the kept `;` on every route a statement can go by — measured through
/// `sp_executesql` (the MERGE test), a plain batch (`BEGIN TRAN;`) and a
/// checked one (`SELECT … INTO #r;`) — and the decisions made from the text
/// are unit-tested with and without it: the gate's charge, the route, the
/// transaction-control match, whether a script is wrapped, and whether a final
/// read may be cut off. The other dialects still drop it: none of their
/// statements needs one.
fn split_into(sql: &str, dialect: Dialect, semicolons: bool) -> Vec<String> {
    let bytes = sql.as_bytes();
    let mut statements = Vec::new();
    let mut body = BodyTracker::new(dialect);
    let mut start = 0usize;
    // True once the current statement has any non-comment, non-whitespace
    // content — comment-only statements are skipped.
    let mut significant = false;
    // Where the current line starts, for GO detection: a separator line has
    // nothing but whitespace before the `GO`. Newlines inside strings,
    // comments, and brackets deliberately do not advance this — the stale
    // prefix then contains non-whitespace (at least the region's opening
    // delimiter), so a `GO` on such a line never splits.
    let mut line_start = 0usize;
    for (range, kind) in lex_regions(sql, dialect) {
        match kind {
            RegionKind::String | RegionKind::Bracket | RegionKind::DollarQuote => {
                significant = true;
                body.feed(BodyToken::Other);
            }
            RegionKind::Comment => {}
            RegionKind::Code => {
                let mut i = range.start;
                while i < range.end {
                    if dialect == Dialect::SqlServer
                        && matches!(bytes[i], b'g' | b'G')
                        && bytes[line_start..i].iter().all(u8::is_ascii_whitespace)
                    {
                        // A GO line is all code (nothing on it can open a
                        // region), so `line_end` never overruns this region.
                        if let Some(line_end) = go_line_end(bytes, i) {
                            if significant {
                                statements.push(sql[start..i].trim().to_string());
                            }
                            start = line_end;
                            line_start = line_end;
                            significant = false;
                            // A batch ends every body and block, balanced or
                            // not, so a miscount cannot leak past a GO.
                            body = BodyTracker::new(dialect);
                            i = line_end;
                            continue;
                        }
                    }
                    let space = unicode_space_len(sql, i, dialect);
                    if space > 0 {
                        i += space;
                        continue;
                    }
                    if dialect == Dialect::SqlServer && bytes[i].is_ascii_digit() {
                        if let Some(number_end) = tsql_number_end(sql, i, range.end) {
                            significant = true;
                            body.feed(BodyToken::Other);
                            i = number_end;
                            // A letter straight after a number starts a word
                            // of its own (`1BEGIN`, `0xABEGIN`).
                            if i < range.end && (bytes[i].is_ascii_alphabetic() || bytes[i] == b'_')
                            {
                                let end = word_end(sql, i, range.end, dialect);
                                body.feed(BodyToken::Word(&sql[i..end]));
                                i = end;
                            }
                            continue;
                        }
                    }
                    if let Some((word_end, bare)) = word_at(sql, i, range.end, dialect) {
                        significant = true;
                        body.feed(if bare {
                            BodyToken::Word(&sql[i..word_end])
                        } else {
                            BodyToken::Other
                        });
                        i = word_end;
                        continue;
                    }
                    match bytes[i] {
                        b';' => {
                            if body.feed(BodyToken::Semicolon) && semicolons {
                                if significant {
                                    // SQL Server keeps the `;` it was split
                                    // on: T-SQL requires one after a MERGE.
                                    let end = if dialect == Dialect::SqlServer {
                                        i + 1
                                    } else {
                                        i
                                    };
                                    statements.push(sql[start..end].trim().to_string());
                                }
                                start = i + 1;
                                significant = false;
                                body = BodyTracker::new(dialect);
                            }
                        }
                        b'\n' => line_start = i + 1,
                        other => {
                            if !other.is_ascii_whitespace() {
                                significant = true;
                                body.feed(BodyToken::Other);
                            }
                        }
                    }
                    i += 1;
                }
            }
        }
    }
    if significant {
        statements.push(sql[start..].trim().to_string());
    }
    statements
}

/// Whether `byte` can continue an identifier: ASCII alphanumerics, `_`, the
/// `$`/`@`/`#` that Postgres and T-SQL allow inside names, and any non-ASCII
/// byte (so a word never ends inside a multibyte character).
fn is_word_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'$' | b'@' | b'#') || byte >= 0x80
}

/// The byte length of a non-ASCII whitespace character at `sql[at..]` that
/// `dialect` treats as whitespace, or 0.
///
/// Only SQL Server does: measured, `SELECT 1<U+00A0>AS x` and the same with
/// U+3000 run there, while Postgres ("trailing junk after numeric literal")
/// and SQLite ("unrecognized token") take the character as part of a name.
fn unicode_space_len(sql: &str, at: usize, dialect: Dialect) -> usize {
    if dialect != Dialect::SqlServer || sql.as_bytes()[at] < 0x80 || !sql.is_char_boundary(at) {
        return 0;
    }
    match sql[at..].chars().next() {
        Some(c) if c.is_whitespace() => c.len_utf8(),
        _ => 0,
    }
}

/// If a word starts at `sql[at..]` — a letter or `_` not continuing an
/// identifier that began earlier — returns where it ends (never past `end`,
/// the end of its code region) and whether it is *bare*.
///
/// A word straight after `.`, `@`, `#` or `$` is not bare: it names a column
/// (`t.end`, legal on Postgres), a T-SQL variable or temp table (`@begin`,
/// `#begin`), so it can never be the keyword it is spelled like. Non-ASCII
/// characters belong to words (`Ébegin` is one identifier), except the
/// whitespace SQL Server treats as such ([`unicode_space_len`]).
fn word_at(sql: &str, at: usize, end: usize, dialect: Dialect) -> Option<(usize, bool)> {
    let bytes = sql.as_bytes();
    let starts = bytes[at].is_ascii_alphabetic() || bytes[at] == b'_';
    let previous = at.checked_sub(1).map(|p| bytes[p]);
    let continues = previous.is_some_and(|b| {
        if b >= 0x80 {
            // The character ending at `at`: a word byte unless it is
            // whitespace to this dialect.
            let start = (0..at)
                .rev()
                .find(|&i| sql.is_char_boundary(i))
                .unwrap_or(0);
            unicode_space_len(sql, start, dialect) == 0
        } else {
            is_word_byte(b) && !matches!(b, b'@' | b'#' | b'$')
        }
    });
    if !starts || continues {
        return None;
    }
    let bare = !matches!(previous, Some(b'.' | b'@' | b'#' | b'$'));
    Some((word_end(sql, at, end, dialect), bare))
}

/// Where the word starting at `sql[at..]` ends (never past `end`). A word
/// runs on through digits and letters alike: an identifier such as
/// `a1begin` is one name to every engine here.
fn word_end(sql: &str, at: usize, end: usize, dialect: Dialect) -> usize {
    let bytes = sql.as_bytes();
    let mut j = at + 1;
    while j < end && is_word_byte(bytes[j]) && unicode_space_len(sql, j, dialect) == 0 {
        j += 1;
    }
    j
}

/// If a T-SQL number literal starts at the ASCII digit `sql[at]`, returns
/// where it ends (never past `end`).
///
/// T-SQL ends a number at the first character that cannot continue it, and
/// a letter there starts the next token: measured on SQL Server 2022,
/// `SELECT 1DELETE FROM t`, `1.5DELETE`, `1e+1DELETE`, `1e1EXEC p` and
/// `0xAUPDATE t SET …` each run the statement after the number. The grammar
/// is digits, an optional `.` and digits, and an optional exponent — an
/// `e`, optional sign and at least one digit — or `0x` and a run of **hex**
/// digits (`0xFDELETE` is `0x0FDE`, then `LETE`).
///
/// A digit starts a number only where a token can start: not after an
/// identifier character, `@` or `#` (`a1begin`, `@1begin` and `#1begin` are
/// names), and not after a `$` that continues a name (`x$1begin`). After any
/// other character it does — a `.` always, and a currency sign, since T-SQL
/// money literals take one (`$1DELETE`, `€1DELETE`, `＄1DELETE` all ran the
/// DELETE). "Identifier character" uses `char::is_alphanumeric`, which also
/// takes `²`, `½` and `①`; measured, `SELECT 1²1DELETE …` (and with `½`,
/// `①`) is a syntax error, so not starting a number after one only spares a
/// statement that cannot run. Only SQL Server reads numbers this way;
/// Postgres and SQLite reject a letter straight after a number.
///
/// **Narrow only what was measured.** Each "does not start a number" above
/// stops the gate charging a word, so each is backed by a measurement; where
/// the engine's reading is unknown, this function picks the reading that
/// splits more.
fn tsql_number_end(sql: &str, at: usize, end: usize) -> Option<usize> {
    let bytes = sql.as_bytes();
    // The character before `at`, and the one before that.
    let before = |i: usize| sql[..i].chars().next_back();
    let in_name = |c: char| c.is_alphanumeric() || matches!(c, '_' | '@' | '#' | '$');
    let starts = match before(at) {
        None => true,
        // `.5` is a literal wherever it stands: after a keyword or a money
        // sign as much as after a space (measured: `SELECT.5DELETE FROM t`,
        // `SELECT DISTINCT.5DELETE …` and `SELECT $.5DELETE …` each ran the
        // DELETE). After a name it is a syntax error (`m.5DELETE`), so
        // starting a number there only over-charges a statement that cannot
        // run.
        Some('.') => true,
        Some('$') => !before(at - 1).is_some_and(in_name),
        Some(c) => !in_name(c),
    };
    if !starts {
        return None;
    }
    let digits = |mut j: usize| {
        while j < end && bytes[j].is_ascii_digit() {
            j += 1;
        }
        j
    };
    if bytes[at] == b'0' && at + 1 < end && matches!(bytes[at + 1], b'x' | b'X') {
        let mut j = at + 2;
        while j < end && bytes[j].is_ascii_hexdigit() {
            j += 1;
        }
        return Some(j);
    }
    let mut j = digits(at);
    if j < end && bytes[j] == b'.' {
        j = digits(j + 1);
    }
    // An exponent only with at least one digit: otherwise the number ends
    // before the `e`, which then starts the next word. Measured, `1EXEC p`
    // and `1EXECUTE p` are syntax errors while `1E+EXEC p` runs the EXEC; the
    // engine's reading of a bare `1E` is not pinned down, and ending the
    // number early is the reading that charges more.
    if j < end && matches!(bytes[j], b'e' | b'E') {
        let mut k = j + 1;
        if k < end && matches!(bytes[k], b'+' | b'-') {
            k += 1;
        }
        if k < end && bytes[k].is_ascii_digit() {
            j = digits(k);
        }
    }
    Some(j)
}

/// One token of a statement, as [`BodyTracker`] sees it. Comments are not
/// tokens at all; a string, bracketed name or dollar quote is one `Other`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BodyToken<'a> {
    /// A bare word (see [`word_at`]), in its original case.
    Word(&'a str),
    Semicolon,
    Other,
}

/// How a statement's `;` are read, decided by its leading words.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BodyRule {
    /// Not decided yet: too few leading words seen.
    Undecided,
    /// Every `;` ends the statement.
    Plain,
    /// SQLite `CREATE [TEMP|TEMPORARY] TRIGGER`: the statement ends only at
    /// `; END ;`, which is `sqlite3_complete`'s own rule. A trigger body
    /// holds nothing but DML statements, so an `END` straight after a `;`
    /// can only close it — a CASE's `END` always follows an expression.
    SqliteTrigger,
    /// SQL Server `CREATE`/`ALTER` [`OR ALTER`] `PROC`/`PROCEDURE`/
    /// `FUNCTION`/`TRIGGER`: the statement runs to the end of the batch, so
    /// no `;` ends it — only GO does. Measured: for a procedure or trigger
    /// the server stores everything up to the batch's end as the body, with
    /// or without `BEGIN … END`. A function's body cannot run past its own
    /// end — a statement after it in the same batch is a server error
    /// ("Incorrect syntax near the keyword 'SELECT'") and nothing is created
    /// — so for functions this is not about the body: keeping the batch whole
    /// sends what SSMS would, and the server reports the mistake itself.
    WholeBatch,
    /// `;` ends the statement only outside `BEGIN … END` blocks (and the
    /// CASEs inside them). Postgres `CREATE [OR REPLACE] FUNCTION`/
    /// `PROCEDURE` (a `BEGIN ATOMIC` body), and every other SQL Server
    /// statement (`IF … BEGIN …; END`, `BEGIN TRY …; END TRY`).
    Blocks,
}

/// Decides, token by token, whether a `;` ends the statement it is in or
/// sits inside a routine or trigger body. Fresh per statement (and per SQL
/// Server batch).
///
/// Errs toward *not* splitting only where that is the server's own unit: on
/// SQL Server a merged statement is just more of the batch SSMS would have
/// sent whole, and GO resets the tracker, so a miscount never crosses a
/// batch. On SQLite the rule is `sqlite3_complete`'s own. On Postgres it
/// starts from psql's and narrows it twice: only `BEGIN ATOMIC` opens a
/// block (`begin` is an unreserved word there and can name a parameter), and
/// a word straight after `AS` is a label, not a keyword. With both, psql's
/// third rule — count nothing inside parentheses — was found to change no
/// legal statement (its one case, `(SELECT 1 AS end)`, is a label), so it is
/// not kept as a guard nothing can trip.
#[derive(Debug)]
struct BodyTracker {
    dialect: Dialect,
    rule: BodyRule,
    /// The statement's first few bare words, lowercased, until the rule is
    /// decided.
    leading: Vec<String>,
    /// Open `BEGIN … END` blocks and CASEs inside them ([`BodyRule::Blocks`]).
    depth: usize,
    /// A keyword whose meaning depends on the word after it.
    pending: Option<Pending>,
    /// The last token was a bare `AS` (Postgres only): the next word is a
    /// column label, which Postgres allows to be any keyword — `SELECT 1 AS
    /// case` and `AS end` are legal (measured) and must count for nothing.
    /// T-SQL reserves BEGIN, CASE and END, so a label there can't be one.
    after_as: bool,
    /// [`BodyRule::SqliteTrigger`]: the last two tokens were `;` (and
    /// then `END`).
    after_semicolon: bool,
    after_semicolon_end: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Pending {
    /// A `BEGIN` that opens a block unless (SQL Server) a transaction or
    /// Service Broker word follows, or only if (Postgres) `ATOMIC` does.
    Begin,
    /// An `END` that closes a block unless `CONVERSATION` follows (T-SQL's
    /// `END CONVERSATION` statement; no such statement exists elsewhere).
    End,
}

/// Words after `BEGIN` that make it a statement of its own on SQL Server
/// rather than the start of a block: `BEGIN TRAN[SACTION]`, `BEGIN
/// DISTRIBUTED TRANSACTION`, `BEGIN DIALOG`, `BEGIN CONVERSATION TIMER`.
const TSQL_BEGIN_STATEMENTS: [&str; 5] = [
    "tran",
    "transaction",
    "distributed",
    "dialog",
    "conversation",
];

/// The [`BodyRule`] a statement opening with `leading` (its lowercased bare
/// words, in order) gets. While `more_may_follow` and `leading` is a prefix
/// of a body-carrying shape, the answer is [`BodyRule::Undecided`].
fn body_rule(dialect: Dialect, leading: &[&str], more_may_follow: bool) -> BodyRule {
    let undecided_or = |rule| {
        if more_may_follow {
            BodyRule::Undecided
        } else {
            rule
        }
    };
    match dialect {
        Dialect::Sqlite => {
            let rest = match leading {
                ["explain", rest @ ..] => rest,
                rest => rest,
            };
            match rest {
                ["create", "trigger", ..] | ["create", "temp" | "temporary", "trigger", ..] => {
                    BodyRule::SqliteTrigger
                }
                [] | ["create"] | ["create", "temp" | "temporary"] => undecided_or(BodyRule::Plain),
                _ => BodyRule::Plain,
            }
        }
        Dialect::Postgres => match leading {
            ["create", "function" | "procedure", ..]
            | ["create", "or", "replace", "function" | "procedure", ..] => BodyRule::Blocks,
            [] | ["create"] | ["create", "or"] | ["create", "or", "replace"] => {
                undecided_or(BodyRule::Plain)
            }
            _ => BodyRule::Plain,
        },
        Dialect::SqlServer => {
            let module = |word: &str| matches!(word, "proc" | "procedure" | "function" | "trigger");
            match leading {
                ["create" | "alter", object, ..] if module(object) => BodyRule::WholeBatch,
                ["create", "or", "alter", object, ..] if module(object) => BodyRule::WholeBatch,
                [] | ["create" | "alter"] | ["create", "or"] | ["create", "or", "alter"] => {
                    undecided_or(BodyRule::Blocks)
                }
                _ => BodyRule::Blocks,
            }
        }
    }
}

impl BodyTracker {
    fn new(dialect: Dialect) -> Self {
        Self {
            dialect,
            rule: BodyRule::Undecided,
            leading: Vec::new(),
            depth: 0,
            pending: None,
            after_as: false,
            after_semicolon: false,
            after_semicolon_end: false,
        }
    }

    /// Takes the statement's next token; returns whether it is a `;` that
    /// ends the statement.
    fn feed(&mut self, token: BodyToken) -> bool {
        if self.rule == BodyRule::Undecided {
            // The leading words end at the first token that is not a word.
            let word = match token {
                BodyToken::Word(word) => Some(word.to_ascii_lowercase()),
                _ => None,
            };
            let more_may_follow = word.is_some();
            self.leading.extend(word);
            let leading: Vec<&str> = self.leading.iter().map(String::as_str).collect();
            self.rule = body_rule(self.dialect, &leading, more_may_follow);
        }
        match self.rule {
            BodyRule::Undecided | BodyRule::Plain => token == BodyToken::Semicolon,
            BodyRule::WholeBatch => false,
            BodyRule::SqliteTrigger => self.sqlite_trigger(token),
            BodyRule::Blocks => self.blocks(token),
        }
    }

    fn sqlite_trigger(&mut self, token: BodyToken) -> bool {
        let ends = token == BodyToken::Semicolon && self.after_semicolon_end;
        let is_end = matches!(token, BodyToken::Word(w) if w.eq_ignore_ascii_case("end"));
        self.after_semicolon_end = self.after_semicolon && is_end;
        self.after_semicolon = token == BodyToken::Semicolon;
        ends
    }

    fn blocks(&mut self, token: BodyToken) -> bool {
        let is =
            |keyword: &str| matches!(token, BodyToken::Word(w) if w.eq_ignore_ascii_case(keyword));
        let labelled = std::mem::replace(
            &mut self.after_as,
            self.dialect == Dialect::Postgres && is("as"),
        );
        if let Some(pending) = self.pending.take() {
            let opens_or_closes = match (pending, self.dialect) {
                (Pending::Begin, Dialect::SqlServer) => {
                    !TSQL_BEGIN_STATEMENTS.iter().any(|w| is(w))
                }
                (Pending::Begin, _) => is("atomic"),
                (Pending::End, _) => !is("conversation"),
            };
            if opens_or_closes {
                match pending {
                    Pending::Begin => self.depth += 1,
                    Pending::End => self.depth = self.depth.saturating_sub(1),
                }
            }
        }
        if token == BodyToken::Semicolon {
            return self.depth == 0;
        }
        if !labelled {
            if is("begin") {
                self.pending = Some(Pending::Begin);
            } else if is("case") && self.depth > 0 {
                self.depth += 1;
            } else if is("end") {
                self.pending = Some(Pending::End);
            }
        }
        false
    }
}

/// If `bytes[at]` (a `g`/`G` with only whitespace before it on its line)
/// starts a SQL Server `GO` separator line, returns the index one past that
/// line (past its newline, or end of input). The line may carry an optional
/// whitespace-separated repeat count (`GO 5`); anything else after the `GO`
/// disqualifies it (`GOTO`, `GO;`, trailing code or comments).
fn go_line_end(bytes: &[u8], at: usize) -> Option<usize> {
    let mut j = at + 1;
    if !matches!(bytes.get(j), Some(b'o' | b'O')) {
        return None;
    }
    j += 1;
    // Optional repeat count: whitespace, then digits.
    let mut k = j;
    while matches!(bytes.get(k), Some(b' ' | b'\t')) {
        k += 1;
    }
    if k > j {
        while matches!(bytes.get(k), Some(c) if c.is_ascii_digit()) {
            k += 1;
        }
    }
    j = k;
    while matches!(bytes.get(j), Some(b' ' | b'\t')) {
        j += 1;
    }
    match bytes.get(j) {
        None => Some(bytes.len()),
        Some(b'\n') => Some(j + 1),
        Some(b'\r') if bytes.get(j + 1) == Some(&b'\n') => Some(j + 2),
        Some(b'\r') => Some(j + 1),
        _ => None,
    }
}

/// If `bytes[at]` opens a dollar-quote delimiter (`$` + optional
/// identifier-like tag + `$`), returns the index one past the opening
/// delimiter. `$1` (a parameter placeholder) is not a delimiter: tags must
/// not start with a digit.
fn dollar_tag_end(bytes: &[u8], at: usize) -> Option<usize> {
    let mut j = at + 1;
    if bytes.get(j) == Some(&b'$') {
        return Some(j + 1); // $$
    }
    // Postgres tag characters: letters, `_` and any non-ASCII byte to start,
    // digits too after that — never `$`.
    if !matches!(bytes.get(j), Some(c) if c.is_ascii_alphabetic() || *c == b'_' || *c >= 0x80) {
        return None;
    }
    while matches!(bytes.get(j), Some(c) if c.is_ascii_alphanumeric() || *c == b'_' || *c >= 0x80) {
        j += 1;
    }
    (bytes.get(j) == Some(&b'$')).then_some(j + 1)
}

/// What a statement does, by first keyword. [`StatementKind::Read`] means it
/// returns rows (executed via fetch); the other two are executed via
/// `execute`, reporting an affected-row count. `Write` and `Ddl` are split
/// because a connection can permit one and refuse the other — a read-only
/// analytics backend refuses both, but the flags are separate in
/// [`Capabilities`](super::caps::Capabilities).
///
/// This is a first-keyword classification only. For the stricter "can this
/// change anything?" question — which also catches writes buried inside a
/// fetch-classified statement — see [`needs_confirmation`] and
/// [`statement_needs`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatementKind {
    Read,
    Write,
    Ddl,
}

impl StatementKind {
    /// Whether the statement is executed by fetching rows rather than by
    /// `execute`. `Ddl` runs exactly like `Write` here — the split matters
    /// for capabilities, not for how the statement is dispatched.
    fn returns_rows(self) -> bool {
        self == StatementKind::Read
    }
}

/// Classifies by the first significant keyword. The read set is
/// deliberately small; anything unrecognized counts as a plain write, which
/// is the safe side (a write needs the broader `mutate` capability, and
/// unknown statements are refused on a read-only connection). `WITH` is
/// classified as a read even though Postgres allows data-modifying CTEs —
/// the rows such a statement returns are still worth showing, and `fetch`
/// executes it all the same (the confirmation banner and the capability
/// check handle the embedded write separately).
pub fn classify_statement(sql: &str, dialect: Dialect) -> StatementKind {
    match first_keyword(sql, dialect).to_ascii_lowercase().as_str() {
        "select" | "with" | "values" | "show" | "explain" | "pragma" | "table" => {
            StatementKind::Read
        }
        word if DDL_KEYWORDS.contains(&word) => StatementKind::Ddl,
        _ => StatementKind::Write,
    }
}

/// Keywords that mark a fetch-classified statement as potentially mutating
/// when they appear anywhere outside strings and comments. Split into the
/// DML and DDL halves so an embedded write can be attributed to the
/// capability it actually needs.
///
/// `truncate` is in both: it is DDL by classification on every engine here,
/// but it also empties the table, so a connection that refuses row changes
/// must refuse it too — otherwise `TRUNCATE t` would wipe every row on a
/// connection where plain `DELETE` is blocked.
///
/// `replace` counts only where it can start SQLite's `REPLACE INTO`
/// statement — see [`has_embedded_dml`]. Followed by `(`, it is the
/// `replace()` string function both SQLite and Postgres have, and a read
/// that calls it changes nothing.
const EMBEDDED_DML_KEYWORDS: [&str; 6] =
    ["insert", "update", "delete", "merge", "replace", "truncate"];
const EMBEDDED_DDL_KEYWORDS: [&str; 4] = ["create", "drop", "alter", "truncate"];

/// [`EMBEDDED_DML_KEYWORDS`] for a dialect. T-SQL has a `REPLACE()` string
/// function and no `REPLACE` statement (measured: `REPLACE INTO t VALUES (1)`
/// is a syntax error on SQL Server 2022), so there the word is a read — and
/// SQL Server scans every read for these words, where `SELECT REPLACE(…)`
/// is common.
fn embedded_dml_keywords(dialect: Dialect) -> &'static [&'static str] {
    match dialect {
        Dialect::SqlServer => &["insert", "update", "delete", "merge", "truncate"],
        Dialect::Sqlite | Dialect::Postgres => &EMBEDDED_DML_KEYWORDS,
    }
}

/// Whether an already-stripped statement holds an [`embedded_dml_keywords`]
/// word for `dialect` — except `replace` straight before a `(` (whitespace
/// and blanked comments between them allowed), which is a call of the string
/// function rather than the statement. SQLite's `REPLACE` statement is always
/// `REPLACE INTO`, so no write is spelled `replace (`; Postgres has no
/// `REPLACE` statement at all, and SQL Server's list leaves the word out
/// already. Matching the word alone refused `WITH x AS (SELECT replace(a,
/// '-', ' ') AS n FROM t) SELECT * FROM x` on a read-only connection while the
/// same query without the CTE ran.
fn has_embedded_dml(stripped: &str, dialect: Dialect) -> bool {
    let keywords = embedded_dml_keywords(dialect);
    let mut rest = stripped;
    while let Some(start) = rest.find(|c: char| c.is_alphanumeric() || c == '_') {
        let after_start = &rest[start..];
        let len = after_start
            .find(|c: char| !(c.is_alphanumeric() || c == '_'))
            .unwrap_or(after_start.len());
        let word = after_start[..len].to_ascii_lowercase();
        rest = &after_start[len..];
        if !keywords.contains(&word.as_str()) {
            continue;
        }
        if word == "replace" && rest.trim_start().starts_with('(') {
            continue;
        }
        return true;
    }
    false
}

/// T-SQL words that change the schema wherever they stand: `GRANT`, `DENY`,
/// `REVOKE`, `TRIGGER` (`ENABLE`/`DISABLE TRIGGER`, which start with words
/// nothing else here knows) and `ADD` (`ADD SIGNATURE`, `ADD COUNTER
/// SIGNATURE`, `ADD SENSITIVITY CLASSIFICATION`). All five are reserved
/// keywords in T-SQL (measured for `ADD`: `SELECT 1 AS add` is a syntax
/// error), so a bare one is never an identifier — which is why `ADD` and not
/// the unreserved `SIGNATURE`, a plausible column name.
const TSQL_EMBEDDED_DDL_KEYWORDS: [&str; 5] = ["grant", "deny", "revoke", "trigger", "add"];

/// T-SQL words that start a statement whose effect nothing here can name,
/// so wherever one appears the statement needs **both** capabilities —
/// whatever else it is charged for.
///
/// This is needed because one hubro statement is not one T-SQL statement:
/// T-SQL needs no `;` between statements, so `SELECT 1 EXEC sp_rename 'a',
/// 'b'`, `BEGIN TRAN EXEC p COMMIT` and a `BEGIN … END` block are each one
/// statement here and several on the server. A first-keyword reading, or a
/// keyword scan that charges only what it can name, sees the half it
/// recognizes and misses the rest.
///
/// The reserved ones (`EXEC`/`EXECUTE`, `DBCC`, `KILL`, `SHUTDOWN`,
/// `RECONFIGURE`, `CHECKPOINT`, `BACKUP`, `RESTORE`, `SETUSER`, `WRITETEXT`,
/// `UPDATETEXT`, and the pass-through functions `OPENQUERY`, `OPENROWSET`,
/// `OPENDATASOURCE`, whose remote text is a string this module blanks before
/// any scan and another server runs) can never be a bare identifier, so no
/// read ever trips them by accident; a linked-server read is charged both,
/// on purpose. `OPENXML` is reserved too but left out: it reads a document
/// already prepared by `sp_xml_preparedocument` (an EXEC, charged) and runs
/// no text of its own. The Service Broker words (`RECEIVE`, `CONVERSATION`, `DIALOG` —
/// `SEND`/`MOVE`/`END CONVERSATION`, `BEGIN DIALOG`, `GET CONVERSATION
/// GROUP` all carry one; `BEGIN DIALOG` may leave out `CONVERSATION`) are
/// not reserved: a column with one of those names makes a read need both
/// too, which is the side to err on.
///
/// What is still not caught: a statement after a read that starts with an
/// unreserved word and carries none of these (none is known that changes
/// data).
const TSQL_OPAQUE_WORDS: [&str; 18] = [
    "exec",
    "execute",
    "dbcc",
    "kill",
    "shutdown",
    "reconfigure",
    "checkpoint",
    "backup",
    "restore",
    "setuser",
    "writetext",
    "updatetext",
    "openquery",
    "openrowset",
    "opendatasource",
    "receive",
    "conversation",
    "dialog",
];

/// Whether an already-stripped statement holds a word that makes it need
/// both capabilities whatever else it is charged for — see
/// [`TSQL_OPAQUE_WORDS`]. Only SQL Server has such words: a Postgres or
/// SQLite statement is one statement (both drivers send it alone), so an
/// opaque statement there is its first keyword, and the unattributed rule in
/// [`statement_needs`] already charges it both.
fn has_opaque_word(dialect: Dialect, stripped: &str) -> bool {
    dialect == Dialect::SqlServer
        && has_word(stripped, |word| {
            let word = word.to_ascii_lowercase();
            TSQL_OPAQUE_WORDS.contains(&word.as_str())
        })
}

/// SQLite pragmas that write to the database file with no argument at all,
/// so the `=`/`(` test cannot see them: `incremental_vacuum` frees pages,
/// `optimize` runs `ANALYZE` (writing `sqlite_stat1`), and `wal_checkpoint`
/// moves the WAL into the database. Charged like `VACUUM` and `ANALYZE`,
/// whose effect nothing here names either: both capabilities.
const SQLITE_WRITE_PRAGMAS: [&str; 3] = ["incremental_vacuum", "optimize", "wal_checkpoint"];

/// SQLite pragmas whose call form reads: the argument names a table, an
/// index or a limit, never a value to set. Measured on SQLite 3.53 against a
/// file opened read-only (each ran) and read-write (none changed a byte):
/// every one of these in `PRAGMA name(arg)` form. Measured the same way, the
/// call form of a setting pragma does write — `user_version(5)`,
/// `application_id(7)`, `journal_mode(WAL)`, `schema_version(9)`,
/// `optimize(…)` were each refused read-only — which is why this is a list of
/// readers rather than "call form is a read".
const SQLITE_READ_CALL_PRAGMAS: [&str; 10] = [
    "table_info",
    "table_xinfo",
    "table_list",
    "index_info",
    "index_xinfo",
    "index_list",
    "foreign_key_list",
    "foreign_key_check",
    "integrity_check",
    "quick_check",
];

/// Whether an already-stripped `PRAGMA` statement is exactly `PRAGMA
/// [schema.]name(argument)` for a name in [`SQLITE_READ_CALL_PRAGMAS`], with
/// a plain word or number for the schema and the argument and nothing after
/// the closing parenthesis. Any other shape is not this form and is left to
/// the value-setting rule, so text this does not understand is charged
/// rather than waved through.
fn is_read_call_pragma(stripped: &str) -> bool {
    // A quoted schema or argument has been blanked to spaces, so "plain" is
    // identifier characters and whitespace — and, for the argument alone, a
    // sign (`integrity_check(-1)`).
    let name_like = |text: &str| {
        text.chars()
            .all(|c| c.is_alphanumeric() || c == '_' || c.is_whitespace())
    };
    let argument_like = |text: &str| {
        text.chars()
            .all(|c| c.is_alphanumeric() || c == '_' || c == '-' || c == '+' || c.is_whitespace())
    };
    let rest = stripped.trim_start();
    let Some(rest) = rest
        .get(..6)
        .filter(|head| head.eq_ignore_ascii_case("pragma"))
        .map(|_| &rest[6..])
    else {
        return false;
    };
    let Some((head, tail)) = rest.split_once('(') else {
        return false;
    };
    let (schema, name) = head.split_once('.').unwrap_or(("", head));
    let name = name.trim().to_ascii_lowercase();
    let Some((argument, after)) = tail.split_once(')') else {
        return false;
    };
    SQLITE_READ_CALL_PRAGMAS.contains(&name.as_str())
        && name_like(schema)
        && argument_like(argument)
        && after.trim().trim_end_matches(';').trim().is_empty()
}

/// First keywords that make a statement schema-changing rather than a plain
/// write. `grant`/`revoke` and `comment` are included: they change the
/// database's definition, not its rows, so a connection that permits data
/// edits but not schema changes must refuse them.
const DDL_KEYWORDS: [&str; 8] = [
    "create", "drop", "alter", "truncate", "rename", "grant", "revoke", "comment",
];

/// Which capabilities a statement needs to run. `read_query` is required by
/// everything (even a write is dispatched through the same connection), so
/// only the two that vary are reported.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct StatementNeeds {
    /// Needs [`Capabilities::mutate`](super::caps::Capabilities::mutate):
    /// the statement can change rows.
    pub mutate: bool,
    /// Needs [`Capabilities::ddl`](super::caps::Capabilities::ddl): the
    /// statement can change the schema.
    pub ddl: bool,
}

/// What `sql` needs from a connection's capabilities.
///
/// Built on the same token scan as [`needs_confirmation`], so it is not
/// fooled by a write hidden in a fetch-classified statement: on a connection
/// that refuses writes, `WITH x AS (DELETE …) SELECT * FROM x` is refused
/// too, not waved through because it starts with `WITH`.
///
/// A statement that [`needs_confirmation`] flags but whose effect no token
/// attributes — `SELECT … INTO new_table`, a value-setting `PRAGMA`, `EXEC
/// sp_rename …`, `CALL some_proc()` — needs **both** capabilities. It can
/// change something, and guessing which of the two would be the one place
/// this module lets an unwanted change through. Note this covers unknown
/// first keywords too: [`classify_statement`] calls them writes for
/// dispatch, but nothing here knows they are *only* writes.
///
/// On SQL Server one statement here can be several on the server, so the
/// rule is additive there: a word from [`TSQL_OPAQUE_WORDS`] anywhere
/// charges both, whatever else the statement is charged for, and only a
/// statement that is transaction control *and nothing else* needs nothing
/// ([`is_bare_transaction_control`]). That list says exactly which forms
/// are still not caught.
///
/// **This gate is not airtight, and cannot be shown to be.** It is a keyword
/// scan over a hand-written lexer that tries to match each engine's own
/// tokenizer ([`region_at`]), and every place the two disagree is a
/// statement the scan cannot see — several such places were found by
/// measuring and are now matched, and nothing proves there are no more. It
/// explains a refusal before anything runs; on SQLite and the Postgres-wire
/// engines a read-only script then runs where the engine refuses writes too
/// ([`ReadOnlyRun`]), so a statement this scan misreads is refused there
/// instead. **On SQL Server it is still the only line** — T-SQL has no
/// read-only mode to fall back on.
pub fn statement_needs(sql: &str, dialect: Dialect) -> StatementNeeds {
    // A SQL Server batch needs what any of its statements does.
    if let Some(statements) = batch_statements(sql, dialect) {
        return statements.iter().map(|s| statement_needs(s, dialect)).fold(
            StatementNeeds::default(),
            |a, b| StatementNeeds {
                mutate: a.mutate || b.mutate,
                ddl: a.ddl || b.ddl,
            },
        );
    }
    // Stripped once here; every scan below works on the stripped form.
    // Each helper used to re-strip the same statement, up to ~6
    // times per capability check.
    let stripped = strip_strings_and_comments(sql, dialect);
    if !confirmation_needed(sql, &stripped, dialect) {
        return StatementNeeds::default();
    }
    let both = StatementNeeds {
        mutate: true,
        ddl: true,
    };
    // Additive, and checked before anything can return less: a statement
    // that runs something nothing here can name needs both, *whatever else*
    // its keywords attribute. A merged T-SQL block such as `BEGIN DELETE
    // FROM a; EXEC sp_rename 'x', 'y'; END` would otherwise be charged only
    // for its DELETE. That includes a SQL Server procedure, function or
    // trigger definition whose body holds an EXEC, although the body does
    // not run at create time: the same over-charge every definition already
    // pays for DML in its body, and the conservative side of it.
    if has_opaque_word(dialect, &stripped) {
        return both;
    }
    let has = |set: &[&str]| {
        has_word(&stripped, |word| {
            set.contains(&word.to_ascii_lowercase().as_str())
        })
    };
    // Both halves are scanned regardless of which keyword opened the
    // statement: DDL can carry DML and vice versa. The first keyword is
    // covered by the same scan (it is a top-level word), so a recognized
    // opener needs no separate check — and an *unrecognized* one is
    // correctly left unattributed rather than assumed to be a row write.
    let mutate = has_embedded_dml(&stripped, dialect);
    let ddl = classify_statement(sql, dialect) == StatementKind::Ddl
        || has(&EMBEDDED_DDL_KEYWORDS)
        || (dialect == Dialect::SqlServer && has(&TSQL_EMBEDDED_DDL_KEYWORDS));
    if !mutate && !ddl {
        // Bare transaction control changes nothing by itself, and the
        // statements it brackets are each checked on their own — so it needs
        // nothing, rather than being refused with a reason that doesn't
        // describe it. "Bare" means the *whole* statement is transaction
        // control (see `is_bare_transaction_control`), not merely that it
        // starts with BEGIN or COMMIT.
        if is_bare_transaction_control(dialect, &stripped) {
            return StatementNeeds::default();
        }
        return both;
    }
    StatementNeeds { mutate, ddl }
}

/// Whether running this statement can mutate the database, i.e. whether the
/// editor must ask before running it. Everything [`classify_statement`]
/// calls a write needs confirmation; on top of that, fetch-classified
/// statements are token-scanned (outside strings/comments, so quoted
/// literals never trigger) for embedded write forms:
///
/// - `WITH` / `EXPLAIN`: any [`EMBEDDED_DML_KEYWORDS`] or
///   [`EMBEDDED_DDL_KEYWORDS`] token anywhere, plus `INTO` —
///   catches data-modifying CTEs (`WITH x AS (DELETE …) SELECT …`),
///   `EXPLAIN ANALYZE UPDATE …`, which Postgres actually executes, and
///   `WITH x AS (…) SELECT * INTO t2 FROM x`, which creates a table just as
///   the bare `SELECT … INTO` form does. Plain `EXPLAIN SELECT` /
///   `EXPLAIN ANALYZE SELECT` do not prompt: the only top-level `INTO` in a
///   read is the table-creating one.
/// - `EXPLAIN` additionally **inherits whatever it explains**: the statement
///   after the `EXPLAIN` header goes through this same check
///   ([`explain_body`]). The keyword scan alone asks only whether a *known*
///   write form appears, so `EXPLAIN CALL do_the_thing()` — a statement whose
///   effect nothing here can name, and which `CALL do_the_thing()` alone is
///   charged both capabilities for — read as a plain query the moment three
///   characters were typed in front of it. The plan view builds
///   exactly such a prefix, so this is what stops it laundering a statement's
///   classification; `explaining_never_loosens_the_gate` pins that as a
///   property rather than a promise.
/// - `SELECT`: an `INTO` token — `SELECT … INTO new_table` creates a table.
/// - `PRAGMA`: a `=` or `(` — the value-setting forms — or one of the
///   [`SQLITE_WRITE_PRAGMAS`] that write with no argument. The call form
///   sets as well as `=` does (`PRAGMA user_version(5)` writes the file), so
///   it is only let through for the [`SQLITE_READ_CALL_PRAGMAS`], whose
///   argument names what to read ([`is_read_call_pragma`]).
/// - **Any read on SQL Server** gets the `WITH` scan, plus
///   [`TSQL_EMBEDDED_DDL_KEYWORDS`] and [`TSQL_OPAQUE_WORDS`]: T-SQL needs
///   no `;` between statements, so `SELECT 1 DELETE FROM t` is a SELECT and
///   a DELETE.
///
/// The dialect matters on SQL Server, where `[bracketed identifiers]` are
/// lexed (see [`strip_strings_and_comments`]): a quote inside one
/// (`SELECT [o'brien] * INTO t2 FROM t`) must not invert string tracking
/// and hide the `INTO`, and `SELECT [into]` must not over-prompt.
pub fn needs_confirmation(sql: &str, dialect: Dialect) -> bool {
    // Not folded over a SQL Server batch's statements, unlike
    // [`statement_needs`]: it would change no answer. A batch led by anything
    // but a read is confirmed, and one led by a read is scanned whole for an
    // embedded write, which finds one in any statement of it.
    confirmation_needed(sql, &strip_strings_and_comments(sql, dialect), dialect)
}

/// [`needs_confirmation`] against an already-stripped statement, so
/// [`statement_needs`] can strip once and share the result with its own
/// keyword scans.
fn confirmation_needed(sql: &str, stripped: &str, dialect: Dialect) -> bool {
    if classify_statement(sql, dialect) != StatementKind::Read {
        return true;
    }
    let embedded_write = || {
        has_embedded_dml(stripped, dialect)
            || has_word(stripped, |word| {
                let word = word.to_ascii_lowercase();
                EMBEDDED_DDL_KEYWORDS.contains(&word.as_str())
                    || word == "into"
                    || (dialect == Dialect::SqlServer
                        && TSQL_EMBEDDED_DDL_KEYWORDS.contains(&word.as_str()))
            })
            || has_opaque_word(dialect, stripped)
    };
    match first_keyword(sql, dialect).to_ascii_lowercase().as_str() {
        "with" => embedded_write(),
        "explain" => {
            // The body is a slice of the *stripped* text, so it is passed as
            // both arguments: strings and comments are already blanked, which
            // is what the `sql` argument is otherwise only used to look
            // through. It is strictly shorter than `stripped` (the `EXPLAIN`
            // keyword is consumed), so the recursion terminates.
            let body = explain_body(stripped);
            embedded_write() || confirmation_needed(body, body, dialect)
        }
        // On SQL Server one statement here can be several on the server — T-SQL
        // needs no `;` between them — so a read is only a read if nothing in
        // it can change anything: `SELECT 1 DELETE FROM t` is a SELECT and a
        // DELETE. Postgres and SQLite drivers send a statement alone, so there
        // the first keyword does speak for all of it.
        _ if dialect == Dialect::SqlServer => embedded_write(),
        "select" => has_word(stripped, |word| word.eq_ignore_ascii_case("into")),
        "pragma" => {
            ((stripped.contains('=') || stripped.contains('(')) && !is_read_call_pragma(stripped))
                || has_word(stripped, |word| {
                    SQLITE_WRITE_PRAGMAS.contains(&word.to_ascii_lowercase().as_str())
                })
        }
        _ => false,
    }
}

/// Words that may stand between `EXPLAIN` and the statement it explains,
/// across the dialects hubro speaks: PostgreSQL's legacy un-parenthesized
/// options (`EXPLAIN ANALYZE VERBOSE SELECT …`, and `ANALYSE`, which its
/// grammar accepts as a synonym) and SQLite's `EXPLAIN QUERY PLAN`. None of
/// them can change anything on its own, which is what makes skipping them
/// safe — a word *not* in this list ends the header and is treated as the
/// start of the explained statement.
///
/// **A word missing here costs a read its capability**, not the other way
/// round: the unrecognized option becomes the first word of the "statement",
/// which classifies as a write, so `EXPLAIN ANALYSE SELECT 1` would be
/// refused on a read-only connection. `explaining_a_read_never_charges_it`
/// is the property that keeps this list honest — a spelling learned later
/// goes into its header list and fails there rather than in front of a user.
const EXPLAIN_OPTION_WORDS: [&str; 5] = ["analyze", "analyse", "verbose", "query", "plan"];

/// The statement an already-stripped `EXPLAIN …` explains: everything after
/// the `EXPLAIN` keyword, its optional parenthesized option list, and any
/// leading [`EXPLAIN_OPTION_WORDS`].
///
/// Degrades toward *more* checking, never less: an unbalanced option list or
/// a header with nothing after it yields an empty body, which
/// [`classify_statement`] calls a write and [`confirmation_needed`] therefore
/// prompts for.
fn explain_body(stripped: &str) -> &str {
    let bytes = stripped.as_bytes();
    let mut i = 0usize;
    // Leading whitespace and open parens, exactly what `first_keyword` skips
    // before reading the `EXPLAIN` it has already matched.
    while i < bytes.len() && (bytes[i].is_ascii_whitespace() || bytes[i] == b'(') {
        i += 1;
    }
    while i < bytes.len() && (bytes[i].is_ascii_alphabetic() || bytes[i] == b'_') {
        i += 1;
    }
    loop {
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        match bytes.get(i) {
            None => break,
            // A parenthesized option list — `(ANALYZE, FORMAT JSON)`. Skipped
            // as a balanced group; an unbalanced one runs to the end and
            // empties the body.
            Some(b'(') => {
                let mut depth = 0usize;
                while i < bytes.len() {
                    match bytes[i] {
                        b'(' => depth += 1,
                        b')' => depth = depth.saturating_sub(1),
                        _ => {}
                    }
                    i += 1;
                    if depth == 0 {
                        break;
                    }
                }
            }
            Some(_) => {
                let start = i;
                while i < bytes.len() && bytes[i].is_ascii_alphabetic() {
                    i += 1;
                }
                let word = stripped[start..i].to_ascii_lowercase();
                if !EXPLAIN_OPTION_WORDS.contains(&word.as_str()) {
                    i = start;
                    break;
                }
            }
        }
    }
    &stripped[i..]
}

/// Whether `sql` is written as an `EXPLAIN`, read as `dialect` reads it. The
/// plan view asks so it can leave such a statement alone instead of
/// prefixing a second `EXPLAIN` onto it, and the script path asks to give its
/// output the plan cell cap ([`cell_cap`]).
///
/// The dialect matters: Postgres and SQL Server nest block comments and
/// SQLite does not, so `/* /* */ EXPLAIN */ SELECT …` is a `SELECT` on the
/// first two. Read as SQLite on every engine, it was left unprefixed by the
/// Explain button — running the `SELECT` instead of explaining it — and its
/// rows were fetched under the plan cap rather than the display cap.
pub fn is_explain(sql: &str, dialect: Dialect) -> bool {
    first_keyword(sql, dialect).eq_ignore_ascii_case("explain")
}

/// Whether any word-ish token (identifier characters) of an
/// already-stripped statement matches the predicate. Takes the
/// [`strip_strings_and_comments`] output rather than stripping itself so
/// callers that run several scans strip once, not once per scan.
fn has_word(stripped: &str, matches: impl Fn(&str) -> bool) -> bool {
    stripped
        .split(|c: char| !(c.is_alphanumeric() || c == '_'))
        .any(|word| !word.is_empty() && matches(word))
}

/// The statement text with everything [`lex_regions`] calls a non-code
/// region — quoted strings and identifiers, comments, dollar quotes — blanked
/// out to spaces, so token scans can't be fooled by literals like `'please do
/// not DELETE me'`, a quote inside a bracketed identifier can't invert string
/// tracking, and a keyword inside one can't masquerade as a token.
///
/// On SQL Server a space is also put between a number literal and a letter
/// straight after it, so word scans see the token boundary the engine sees
/// ([`tsql_number_end`]: `SELECT 1DELETE FROM t` and `0xAUPDATE t SET …`
/// each run the write). That is the one case where the result is longer
/// than `sql`. Names keep their digits: `@1delete` or `tbl1delete` is one
/// word, as it is to the engine. Postgres and SQLite reject a letter after a
/// number ("trailing junk" / "unrecognized token"), so they keep the text
/// as is.
fn strip_strings_and_comments(sql: &str, dialect: Dialect) -> String {
    let bytes = sql.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    for (range, kind) in lex_regions(sql, dialect) {
        if kind != RegionKind::Code {
            out.resize(out.len() + range.len(), b' ');
            continue;
        }
        let mut i = range.start;
        while i < range.end {
            if dialect == Dialect::SqlServer && bytes[i].is_ascii_digit() {
                if let Some(number_end) = tsql_number_end(sql, i, range.end) {
                    out.extend_from_slice(&bytes[i..number_end]);
                    if number_end < range.end
                        && (bytes[number_end].is_ascii_alphabetic() || bytes[number_end] == b'_')
                    {
                        out.push(b' ');
                    }
                    i = number_end;
                    continue;
                }
            }
            out.push(bytes[i]);
            i += 1;
        }
    }
    // Multibyte chars survive intact: regions always start and end at ASCII
    // delimiters (a UTF-8 continuation byte can never equal an ASCII
    // quote/comment marker), so a code region copied here is never split
    // mid-sequence. The output has the input's byte length, except on SQL
    // Server, where a space put after a number literal adds one byte each
    // time — always before an ASCII letter, never inside a character.
    String::from_utf8_lossy(&out).into_owned()
}

/// The first keyword of a statement, skipping leading whitespace, comments,
/// and opening parentheses (`(SELECT …)` is a read). Anything else — a
/// string, a dollar quote, a non-keyword code byte — ends the scan: a
/// statement opening with one has no first keyword and classifies as a
/// write, the safe side.
///
/// Lexes with the statement's dialect, because where a comment ends is the
/// engine's own rule: lexed as SQLite, a Postgres `--c<CR>WITH e AS (DELETE
/// …) …<LF>SELECT …` read as a SELECT whose comment hid the DELETE.
pub(super) fn first_keyword(sql: &str, dialect: Dialect) -> &str {
    let bytes = sql.as_bytes();
    for (range, kind) in lex_regions(sql, dialect) {
        match kind {
            RegionKind::Comment => {}
            RegionKind::Code => {
                let mut i = range.start;
                while i < range.end && (bytes[i].is_ascii_whitespace() || bytes[i] == b'(') {
                    i += 1;
                }
                if i == range.end {
                    continue; // nothing but whitespace/parens: keep looking
                }
                let start = i;
                while i < range.end && (bytes[i].is_ascii_alphabetic() || bytes[i] == b'_') {
                    i += 1;
                }
                return &sql[start..i];
            }
            _ => return "",
        }
    }
    ""
}

/// Whether `sql` ends inside a `--` line comment on `dialect`, so that a `;`
/// appended to it would be commented out. Only the last region decides: a
/// comment closed earlier in the text says nothing about the end, and where
/// a line comment stops (`\n`, or a bare `\r` off SQLite) is the engine's own
/// rule, which [`region_at`] already keeps.
pub(super) fn ends_in_line_comment(sql: &str, dialect: Dialect) -> bool {
    lex_regions(sql, dialect)
        .last()
        .is_some_and(|(range, kind)| kind == RegionKind::Comment && sql[range].starts_with("--"))
}

/// How many block comments are still open where `sql` ends on `dialect`: 0
/// unless the last region is a `/* … */` that never closed, and more than 1
/// only where comments nest (Postgres, SQL Server: `/* a /* b` leaves two).
/// SQLite accepts one at the end of input and stores it with an index, view
/// or `STRICT` table (`CREATE INDEX i ON t(a) /* note`), and a `;` appended
/// to it, on any line, is still inside it. (Postgres refuses the text:
/// "unterminated /* comment".)
pub(super) fn open_block_comments_at_end(sql: &str, dialect: Dialect) -> usize {
    match lex_regions(sql, dialect).last() {
        Some((range, RegionKind::Comment)) if sql[range.clone()].starts_with("/*") => {
            block_comment_end(sql.as_bytes(), range.start, dialect).1
        }
        _ => 0,
    }
}

/// Whether `sql` already ends in a `;` that `dialect` reads as a statement
/// terminator — not one at the end of a comment, string or quoted name.
pub(super) fn ends_with_terminator(sql: &str, dialect: Dialect) -> bool {
    lex_regions(sql, dialect)
        .last()
        .is_some_and(|(range, kind)| {
            kind == RegionKind::Code && sql[range].trim_end().ends_with(';')
        })
}

/// Maximum characters in a statement preview.
const PREVIEW_CHARS: usize = 60;

/// A short, single-line form of a statement for result headers: whitespace
/// runs collapse to one space, and anything past [`PREVIEW_CHARS`]
/// characters is cut with an ellipsis (at a char boundary).
pub fn statement_preview(sql: &str) -> String {
    let mut collapsed = String::new();
    let mut last_was_space = false;
    for c in sql.trim().chars() {
        if c.is_whitespace() {
            if !last_was_space {
                collapsed.push(' ');
            }
            last_was_space = true;
        } else {
            collapsed.push(c);
            last_was_space = false;
        }
    }
    match collapsed.char_indices().nth(PREVIEW_CHARS) {
        Some((cut, _)) => {
            collapsed.truncate(cut);
            collapsed.push('…');
            collapsed
        }
        None => collapsed,
    }
}

/// What one successfully executed statement produced.
#[derive(Debug, Clone, PartialEq)]
pub enum StatementOutcome {
    /// A read: the fetched rows.
    Rows(QueryResult),
    /// A write: what the driver reported about the rows it affected.
    Affected(RowsAffected),
}

/// What a statement that returns no rows reported about the rows it affected
/// — the number the result line states, and how far it can be read as the
/// statement's own.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowsAffected {
    /// One count, the statement's own. Every SQLite and Postgres write: their
    /// counts (`changes()`, the command tag) already leave trigger work out.
    Exact(u64),
    /// Several counts added up: a SQL Server statement whose response carried
    /// more than one — several T-SQL statements in one (an `EXEC`, a
    /// `BEGIN … END`, statements written without semicolons), or one whose
    /// triggers reported counts of their own. Which is which cannot be told
    /// from the counts (`script_rows_affected` in the SQL Server backend), so
    /// the result line says the sum may hold trigger rows.
    Summed(u64),
    /// No count at all: a SQL Server statement sent as a plain batch, which
    /// the driver reads no counts from, or one run under `SET NOCOUNT ON`.
    /// Reported as no number rather than as 0 — a `SELECT … INTO #t` that
    /// copied rows went that way.
    Unreported,
}

impl RowsAffected {
    /// The number reported, and 0 for [`RowsAffected::Unreported`] — for the
    /// callers that only ever held a count.
    pub fn rows(self) -> u64 {
        match self {
            RowsAffected::Exact(n) | RowsAffected::Summed(n) => n,
            RowsAffected::Unreported => 0,
        }
    }
}

/// What a statement run as a read produced. On SQL Server, a read that
/// came back with no result set at all was not a read: `SELECT … INTO`
/// (classified by its `SELECT`) fills a table and returns nothing, and the
/// count it sends is lost on the read path — tiberius's row stream drops
/// counts, and a temp-table creator runs as a plain batch, which carries
/// none. Measured, `SELECT a INTO #t FROM t` over three rows was shown as
/// "0 rows" and "The statement returned no rows." It is reported as a write
/// that stated no count instead. A T-SQL `SELECT` always has a column, so
/// no real result set looks like this. Other engines keep their rows as
/// fetched: Postgres can return a result set of no columns (`SELECT FROM t`).
fn read_outcome(dialect: Dialect, result: QueryResult) -> StatementOutcome {
    if dialect == Dialect::SqlServer && result.columns.is_empty() && result.rows.is_empty() {
        StatementOutcome::Affected(RowsAffected::Unreported)
    } else {
        StatementOutcome::Rows(result)
    }
}

/// One executed statement's result: its preview (for the result header) plus
/// its outcome. A statement that returned several result sets — on SQL Server
/// one statement can — reports one of these per set, in order, sharing
/// `statement` and told apart by `set`.
#[derive(Debug, Clone, PartialEq)]
pub struct StatementResult {
    /// Which statement of the script this came from, 0-based — the same
    /// numbering a failure's `statement_index` uses.
    pub statement: usize,
    /// Which of the statement's result sets this is, when more than one was
    /// kept. `None` for a statement that returned one result — or that was
    /// cut short at the row cap inside its first set, past which hubro
    /// cannot say whether more sets followed.
    pub set: Option<SetOf>,
    pub preview: String,
    pub outcome: StatementOutcome,
    /// True when a read hit the [`MAX_QUERY_ROWS`] fetch cap and its result
    /// holds only the first N rows — the editor shows a "showing
    /// first N" indicator. Always false for writes.
    pub truncated: bool,
    /// What became of the rest of a truncated read — whether the server
    /// stopped the statement at the cap, and on what grounds. The editor says
    /// so beside the truncation notice.
    pub rest: RestOfRead,
}

/// Where one result set falls among its statement's: the `index`th
/// (0-based) of `count`. `count` is known only when the statement's results
/// were read whole: a read cut short at the row cap stops before the sets
/// after the cut, so how many there were is not known, and it is `None`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SetOf {
    pub index: usize,
    pub count: Option<usize>,
}

/// A read's outcome as a run reports it: one entry per result set, each with
/// its rows, whether the row cap cut it short, and whether its rest was then
/// stopped. Only the last set can be either; the sets before it were read
/// whole.
fn rows_outcomes(dialect: Dialect, capped: Capped) -> Vec<(StatementOutcome, bool, RestOfRead)> {
    capped
        .earlier
        .into_iter()
        .map(|set| (read_outcome(dialect, set), false, RestOfRead::Ran))
        .chain(std::iter::once((
            read_outcome(dialect, capped.result),
            capped.truncated,
            capped.rest,
        )))
        .collect()
}

/// What a failed script's rollback actually undid — the state the editor
/// reports so the user knows what is in the database.
///
/// Three states rather than a bool because there are three, and the one a
/// bool used to hide is the one that misleads: a rollback that covered the
/// data but not the schema was reported as if it had covered everything.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rollback {
    /// Nothing was undone. The sequential autocommit path, or a failure
    /// before the transaction opened — statements before the failure stand.
    None,
    /// The atomic path found hubro's transaction already over when it went
    /// to roll back ([`RolledBack::Nothing`]): a statement committed it (a
    /// `COMMIT` inside a SQL Server batch, or in a trigger), or the server
    /// rolled it back (a deadlock, `XACT_ABORT`). Hubro undid nothing and
    /// cannot tell which, so what stands — possibly part of the failing
    /// statement itself — is the server error's to say.
    AlreadyEnded,
    /// Everything the script had run was undone.
    Full,
    /// The transaction rolled back, but the connection has no
    /// [`transactional_ddl`](super::caps::Capabilities::transactional_ddl)
    /// and the script changes the schema, so the rollback could not cover all
    /// of it.
    ///
    /// Says "at least the schema changes escaped" rather than naming exactly
    /// what survived, because that differs by engine and hubro would have to
    /// guess: CockroachDB also commits everything *before* each DDL statement,
    /// while YugabyteDB rolls that DML back normally. Claiming less than is
    /// known beats claiming more — the mistake this variant exists to stop.
    ExceptSchemaChanges,
    /// The script had opened a transaction of its own (`BEGIN`) and failed
    /// inside it, and hubro rolled that transaction back when the run ended.
    /// The sequential path, which runs such scripts, reports it, and so does
    /// the atomic path when a SQL Server batch committed hubro's transaction
    /// and opened another ([`RolledBack::Another`]).
    ///
    /// What the rollback covered is the script's transaction, not the
    /// script: statements before its `BEGIN`, and anything it had committed,
    /// stand.
    OpenTransaction(OpenTransaction),
    /// Hubro could not check what the script left open — the sequential
    /// path's check failed, or the atomic path's connection was lost
    /// ([`RolledBack::Unknown`]) — before the connection closed, as every
    /// run's does. The server discards anything uncommitted on a closed
    /// connection; whether there was anything, hubro cannot say.
    ConnectionClosed,
}

/// A transaction a script opened itself and had not ended when the run did,
/// which hubro rolled back before closing the connection, so the run can say
/// so — see [`ScriptConn`](super::registry::ScriptConn).
///
/// Rolled back, not committed and not kept open for a later run: a script
/// that ends without its `COMMIT` has not said its changes are done, and
/// rolling back is the one ending that leaves nothing the user did not ask
/// for. Every change made since its `BEGIN` is discarded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OpenTransaction {
    /// The connection has no
    /// [`transactional_ddl`](super::caps::Capabilities::transactional_ddl) and
    /// the run changed the schema, so the rollback may not have covered all of
    /// the transaction — as [`Rollback::ExceptSchemaChanges`] says for hubro's
    /// own.
    pub schema_changes_kept: bool,
}

/// A script that ran every statement.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ScriptDone {
    /// Set when the script ended inside a transaction it had opened itself
    /// (a `BEGIN` with no `COMMIT`) and hubro rolled it back, so the run is
    /// not mistaken for one whose changes took effect.
    pub rolled_back: Option<OpenTransaction>,
    /// Hubro could not check whether the script left a transaction open
    /// before the connection closed — see [`Rollback::ConnectionClosed`].
    pub connection_closed: bool,
}

/// Where and how a script failed.
#[derive(Debug, Clone, PartialEq)]
pub struct ScriptError {
    /// Index into the script's statement list.
    pub statement_index: usize,
    /// Preview of the failing statement.
    pub preview: String,
    pub error: DbError,
    /// What the rollback undid — see [`Rollback`]. The editor surfaces this so
    /// the user knows the database state.
    pub rollback: Rollback,
}

/// Runs a script's statements, calling `on_result` after each successful
/// statement (so callers can show progress) and stopping at the first failure.
///
/// Multi-statement scripts run atomically in one transaction unless
/// [`wrap_atomically`] declines (self-managed transactions, a
/// non-transactional statement, or a connection without the `transactions`
/// capability) — then they run sequentially in autocommit, still on one
/// connection ([`ScriptConn`](super::registry::ScriptConn)), and a transaction the script leaves open is
/// rolled back and reported ([`ScriptDone::rolled_back`],
/// [`Rollback::OpenTransaction`]).
///
/// Every statement is checked against `caps` *before* any of them runs,
/// so a script that the UI should have gated fails with a clear
/// [`DbError::Unsupported`] and an untouched database, rather than part-way
/// through. The UI disables the run affordance for these cases; this is the
/// backstop for reaching the path anyway.
///
/// `caps` is passed in rather than read from `pool` because the connection's
/// *effective* capabilities are the backend's narrowed by the user's write
/// protection — see
/// [`Connection::capabilities`](super::registry::Connection::capabilities).
/// Taking it as an argument means a caller cannot accidentally consult the
/// engine's own answer and skip the marking.
///
/// It is also read **per run**, which is what makes a change of marking take
/// effect on an open tab without reconnecting: a connection that may not
/// write ([`engine_enforces_read_only`]) runs the whole script through a
/// [`ReadOnlyRun`], where the engine refuses a write the gate missed. Nothing
/// about the pool itself changes with the marking, so there is nothing to
/// re-apply. SQL Server has no such mode; there the gate is all there is.
pub async fn run_script(
    pool: &DbPool,
    caps: Capabilities,
    statements: &[String],
    on_result: impl FnMut(StatementResult),
) -> Result<ScriptDone, ScriptError> {
    run_script_watching_rest(pool, caps, statements, on_result)
        .await
        .0
}

/// [`run_script`], and the outcome of a last read whose rest is read after
/// the run ([`RestOfRead::Unconfirmed`]) — `Some` exactly when that read's
/// result says so. The run reports as soon as its statements have; the
/// [`PendingRest`] resolves when the background read ends, so whoever shows
/// the run can say how the rest went.
pub async fn run_script_watching_rest(
    pool: &DbPool,
    caps: Capabilities,
    statements: &[String],
    on_result: impl FnMut(StatementResult),
) -> (Result<ScriptDone, ScriptError>, Option<PendingRest>) {
    if let Some(refusal) = refuse_script(caps, statements, pool.dialect()) {
        return (Err(refusal), None);
    }
    if engine_enforces_read_only(caps) {
        match pool.begin_read_only_run().await {
            Ok(Some(run)) => {
                let outcome = run_script_read_only(run, pool.dialect(), statements, on_result)
                    .await
                    .map(|()| ScriptDone::default());
                return (outcome, None);
            }
            // SQL Server: no read-only mode to run in, so the script runs as
            // any other does, behind the gate alone.
            Ok(None) => {}
            Err(error) => return (Err(nothing_ran(statements, error)), None),
        }
    }
    if wrap_atomically(caps, pool.dialect(), statements) {
        let outcome = run_script_atomic(pool, caps, statements, on_result)
            .await
            .map(|()| ScriptDone::default());
        (outcome, None)
    } else {
        run_script_sequential(pool, caps, statements, on_result).await
    }
}

/// What a rollback covered, given the statements that actually *ran*.
///
/// [`Rollback::ExceptSchemaChanges`] needs both halves: an engine whose
/// rollback lets DDL escape, *and* a schema change among the statements that
/// reached the server. A pure-DML script on CockroachDB rolls back completely,
/// and saying otherwise would be its own false claim — the opposite one, and
/// no better.
///
/// **`ran` is the statements up to and including the failing one, not the whole
/// script.** A script whose `CREATE TABLE` sits *after* the statement that
/// failed never executed that DDL, so its rollback really did cover everything;
/// reporting otherwise would invent a surviving table. The failing statement is
/// *included* because a DDL that fails has still done its damage on
/// CockroachDB: `autocommit_before_ddl` commits the open transaction before the
/// statement runs, so the writes staged ahead of it survive even when the
/// schema change itself does not.
///
/// Uses [`statement_needs`] rather than [`classify_statement`] for the same
/// reason the capability gate does: it catches a schema change that the first
/// keyword doesn't advertise (`SELECT … INTO new_table`, `EXEC sp_rename`),
/// which the conservative side here counts as DDL.
fn rollback_cover(caps: Capabilities, dialect: Dialect, ran: &[String]) -> Rollback {
    let changes_schema = || ran.iter().any(|s| statement_needs(s, dialect).ddl);
    if caps.transactional_ddl || !changes_schema() {
        Rollback::Full
    } else {
        Rollback::ExceptSchemaChanges
    }
}

/// Why a script holding a NUL character is refused — see [`script_refusal`].
pub const NUL_IN_SCRIPT: &str =
    "This script contains a NUL character, which can't be sent to the database.";

/// The first statement of `statements` that `caps` doesn't permit: its index
/// and the sentence explaining the refusal, or `None` when the whole script
/// may run.
///
/// The UI calls this before offering to run a script — so a script that
/// can't run is refused up front rather than after a write-confirmation
/// prompt the user can only answer wrongly — and [`run_script`] calls it
/// again as the backstop.
pub fn script_refusal(
    caps: Capabilities,
    statements: &[String],
    dialect: Dialect,
) -> Option<(usize, &'static str)> {
    // Before anything else, and whatever the capabilities: measured,
    // sqlx-sqlite spins forever at full CPU on a statement followed by a NUL
    // (one inside a quoted token fails cleanly; the SQLite backend refuses
    // both, see `SqlText`), SQL Server runs straight past one, and Postgres
    // fails the protocol message. Nothing a script needs is spelled with one.
    if let Some(statement_index) = statements.iter().position(|s| s.contains('\0')) {
        return Some((statement_index, NUL_IN_SCRIPT));
    }
    if !caps.read_query && !statements.is_empty() {
        // Nothing reaches the connection at all, so the first statement
        // carries the refusal.
        return Some((0, caps::NO_QUERY));
    }
    for (statement_index, statement) in statements.iter().enumerate() {
        let needs = statement_needs(statement, dialect);
        if needs.mutate && !caps.mutate {
            return Some((statement_index, caps::NO_MUTATE));
        }
        if needs.ddl && !caps.ddl {
            return Some((statement_index, caps::NO_DDL));
        }
    }
    None
}

/// [`script_refusal`] as a ready-to-return error. Nothing has run at this
/// point, so the rollback is [`Rollback::None`]: there was nothing to undo,
/// and the database is untouched either way.
fn refuse_script(
    caps: Capabilities,
    statements: &[String],
    dialect: Dialect,
) -> Option<ScriptError> {
    script_refusal(caps, statements, dialect).map(|(statement_index, message)| ScriptError {
        statement_index,
        preview: statement_preview(&statements[statement_index]),
        error: DbError::Unsupported(message.to_string()),
        rollback: Rollback::None,
    })
}

/// The per-cell byte cap for one statement's rows on the script path:
/// [`PLAN_CELL_CAP`] for an `EXPLAIN`, whose output is a plan document that a
/// prefix of cannot be parsed or read, and [`QUERY_CELL_CAP`] for everything
/// else — the display cap ordinary queries keep. Decided by the statement
/// rather than by the Explain button, so a plan the user typed and ran is
/// whole too — and read in the engine's own dialect, so a statement that
/// only *looks* like an `EXPLAIN` behind a nested comment keeps the display
/// cap ([`is_explain`]).
fn cell_cap(statement: &str, dialect: Dialect) -> usize {
    if is_explain(statement, dialect) {
        PLAN_CELL_CAP
    } else {
        QUERY_CELL_CAP
    }
}

/// The run's hold on its connection could not be had: nothing ran, so
/// nothing rolled back, and the first statement carries the error.
fn nothing_ran(statements: &[String], error: DbError) -> ScriptError {
    ScriptError {
        statement_index: 0,
        preview: statements
            .first()
            .map(|s| statement_preview(s))
            .unwrap_or_default(),
        error,
        rollback: Rollback::None,
    }
}

/// One script run's hold on the server, as the statement loop
/// ([`run_statements`]) drives it: a [`ScriptConn`] (sequential), a
/// [`ReadOnlyRun`] (marked read-only) or a [`ScriptTx`] (atomic). The loop is
/// the same for all three; what differs is in here, and in what each run
/// makes of the loop's outcome afterwards ([`sequential_outcome`],
/// [`read_only_outcome`], [`atomic_outcome`]).
trait StatementExecutor: Sized {
    /// How letting go of the hold went.
    type Ended;

    /// Whether the script's last statement, when it reads, lets go of the
    /// hold with it ([`Self::finish_with_read`]) rather than being reported
    /// first and the hold let go after ([`Self::end`]). Stated by each hold
    /// ([`step`] reads it): `true` where the read's end is how a cut-short
    /// read gets closed rather than read to its end (`ScriptConn`,
    /// `ReadOnlyRun`); `false` for the atomic run, whose last read is
    /// reported before the commit — so the editor shows it while the commit
    /// runs, and a run cancelled during the commit has shown it already.
    const ENDS_WITH_FINAL_READ: bool;

    /// Runs a row-returning statement, capped.
    async fn read(&mut self, sql: &str, max_rows: u64, cell_cap: usize) -> Result<Capped, DbError>;

    /// Runs any other statement, returning its affected-row count.
    async fn write(&mut self, sql: &str) -> Result<RowsAffected, DbError>;

    /// Lets go of the hold once the statements are done. `failed` is whether
    /// one of them failed — the atomic run rolls back then and commits
    /// otherwise; the other two let go the same way either way.
    async fn end(self, failed: bool) -> Self::Ended;

    /// Runs the script's last statement, a read, and lets go of the hold with
    /// it — so a hold that would otherwise wait for the rest of a cut-short
    /// result can close instead. Called only where
    /// [`Self::ENDS_WITH_FINAL_READ`]. No default: a hold that ends with its
    /// final read says how, rather than inheriting "read, then end".
    async fn finish_with_read(
        self,
        sql: &str,
        max_rows: u64,
        cell_cap: usize,
    ) -> (Result<Capped, DbError>, Self::Ended);
}

/// The sequential run: every statement on the one connection the run holds,
/// and letting go of it rolls back whatever transaction the script left open
/// ([`ScriptConn::end`]), whether a statement failed or not.
impl StatementExecutor for ScriptConn {
    /// How ending the hold went, and the outcome of a rest read after the
    /// run, if the last read left one.
    type Ended = (Result<bool, DbError>, Option<PendingRest>);
    const ENDS_WITH_FINAL_READ: bool = true;

    async fn read(&mut self, sql: &str, max_rows: u64, cell_cap: usize) -> Result<Capped, DbError> {
        self.query_capped_to(sql, max_rows, cell_cap).await
    }

    async fn write(&mut self, sql: &str) -> Result<RowsAffected, DbError> {
        self.execute(sql).await
    }

    async fn end(self, _failed: bool) -> Self::Ended {
        (ScriptConn::end(self).await, None)
    }

    async fn finish_with_read(
        self,
        sql: &str,
        max_rows: u64,
        cell_cap: usize,
    ) -> (Result<Capped, DbError>, Self::Ended) {
        let (read, ended, pending) =
            ScriptConn::finish_with_read(self, sql, max_rows, cell_cap).await;
        (read, (ended, pending))
    }
}

/// The read-only run: letting go closes the connection, failed or not.
impl StatementExecutor for ReadOnlyRun {
    type Ended = ();
    const ENDS_WITH_FINAL_READ: bool = true;

    async fn read(&mut self, sql: &str, max_rows: u64, cell_cap: usize) -> Result<Capped, DbError> {
        self.query_capped_to(sql, max_rows, cell_cap).await
    }

    async fn write(&mut self, sql: &str) -> Result<RowsAffected, DbError> {
        self.execute(sql).await
    }

    async fn end(self, _failed: bool) {
        self.close().await;
    }

    async fn finish_with_read(
        self,
        sql: &str,
        max_rows: u64,
        cell_cap: usize,
    ) -> (Result<Capped, DbError>, ()) {
        (
            ReadOnlyRun::finish_with_read(self, sql, max_rows, cell_cap).await,
            (),
        )
    }
}

/// How an atomic run let go of its transaction.
#[derive(Debug)]
enum TxEnded {
    /// A statement failed, and the transaction was rolled back; what the
    /// rollback found open.
    RolledBack(RolledBack),
    /// Every statement ran, and the transaction was committed — or the
    /// commit failed, and what the rollback after it found.
    Committed(Result<(), (DbError, RolledBack)>),
}

/// Whether an atomic run commits: only when no statement failed.
fn tx_commits(failed: bool) -> bool {
    !failed
}

/// The atomic run: the statements share one transaction, committed only if
/// all of them ran ([`tx_commits`]). Its last read is an ordinary one,
/// reported before the commit (`ENDS_WITH_FINAL_READ` is `false`).
impl StatementExecutor for ScriptTx<'_> {
    type Ended = TxEnded;
    const ENDS_WITH_FINAL_READ: bool = false;

    async fn read(&mut self, sql: &str, max_rows: u64, cell_cap: usize) -> Result<Capped, DbError> {
        self.query_capped_to(sql, max_rows, cell_cap).await
    }

    async fn write(&mut self, sql: &str) -> Result<RowsAffected, DbError> {
        self.execute(sql).await
    }

    async fn end(self, failed: bool) -> TxEnded {
        if tx_commits(failed) {
            TxEnded::Committed(self.commit().await)
        } else {
            TxEnded::RolledBack(self.rollback().await)
        }
    }

    /// Never called: the atomic run's last read is reported before its
    /// commit (`ENDS_WITH_FINAL_READ` is `false`).
    async fn finish_with_read(
        self,
        _: &str,
        _: u64,
        _: usize,
    ) -> (Result<Capped, DbError>, TxEnded) {
        unreachable!("an atomic run does not end with its final read")
    }
}

/// How the statement loop runs one statement: what [`run_statements`] does
/// with it, over plain data.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Step {
    /// A read that is not the script's last.
    Read,
    /// The script's last statement, a read, on a hold that lets go with it:
    /// run it and let go of the hold together
    /// ([`StatementExecutor::finish_with_read`]), then report it.
    FinishWithRead,
    /// Anything that does not return rows.
    Write,
}

fn step(ends_with_final_read: bool, reads: bool, is_last: bool) -> Step {
    match (reads, is_last && ends_with_final_read) {
        (true, true) => Step::FinishWithRead,
        (true, false) => Step::Read,
        (false, _) => Step::Write,
    }
}

/// The statement loop every script run goes through: each statement in
/// order through `exec`, reporting each outcome to `on_result`, **stopping
/// at the first failure** — no statement after a failed one is sent, in any
/// mode — and then letting go of the hold, told whether a statement failed.
/// A SQL Server unit is a whole batch, though, and the server decides what
/// runs inside one: it carries on past most errors, so statements after the
/// error in the same batch may have run.
/// Returns the failure (its index and error) and how letting go went.
///
/// Reads stream through the row cap so a huge result never buffers in full;
/// writes report an affected-row count.
async fn run_statements<E: StatementExecutor>(
    exec: E,
    dialect: Dialect,
    statements: &[String],
    mut on_result: impl FnMut(StatementResult),
) -> (Option<(usize, DbError)>, E::Ended) {
    let last = statements.len().saturating_sub(1);
    let mut exec = Some(exec);
    // Set when the last statement let go of the hold itself.
    let mut ended = None;
    let mut failure = None;
    for (statement_index, statement) in statements.iter().enumerate() {
        let reads = returns_rows(statement, dialect);
        let cap = cell_cap(statement, dialect);
        let outcome = match step(E::ENDS_WITH_FINAL_READ, reads, statement_index == last) {
            Step::FinishWithRead => {
                let (read, end) = exec
                    .take()
                    .expect("held until the last statement")
                    .finish_with_read(statement, MAX_QUERY_ROWS, cap)
                    .await;
                ended = Some(end);
                read.map(|capped| rows_outcomes(dialect, capped))
            }
            Step::Read => exec
                .as_mut()
                .expect("held until the last statement")
                .read(statement, MAX_QUERY_ROWS, cap)
                .await
                .map(|capped| rows_outcomes(dialect, capped)),
            Step::Write => exec
                .as_mut()
                .expect("held until the last statement")
                .write(statement)
                .await
                .map(|affected| {
                    vec![(StatementOutcome::Affected(affected), false, RestOfRead::Ran)]
                }),
        };
        match outcome {
            Ok(sets) => {
                let kept = sets.len();
                // Only the last set can be cut short, and a cut hides how
                // many sets came after it.
                let cut = sets.last().is_some_and(|&(_, truncated, _)| truncated);
                let count = (!cut).then_some(kept);
                for (index, (outcome, truncated, rest)) in sets.into_iter().enumerate() {
                    on_result(StatementResult {
                        statement: statement_index,
                        set: (kept > 1).then_some(SetOf { index, count }),
                        preview: statement_preview(statement),
                        outcome,
                        truncated,
                        rest,
                    });
                }
            }
            Err(error) => {
                failure = Some((statement_index, error));
                break;
            }
        }
    }
    let ended = match (ended, exec) {
        (Some(ended), _) => ended,
        (None, Some(exec)) => exec.end(failure.is_some()).await,
        (None, None) => unreachable!("the hold is let go by the last statement or here"),
    };
    (failure, ended)
}

/// The autocommit path: each statement commits on its own, so a failure leaves
/// earlier statements' effects in place ([`Rollback::None`]) — unless the
/// script had opened a transaction of its own and was inside it, which
/// [`ScriptConn::end`](super::registry::ScriptConn::end) rolls back ([`Rollback::OpenTransaction`]).
///
/// Every statement runs on the one connection the run holds: a script's own
/// `BEGIN` and the statements after it must share a session, or the `BEGIN`
/// opens a transaction nothing else runs in.
async fn run_script_sequential(
    pool: &DbPool,
    caps: Capabilities,
    statements: &[String],
    on_result: impl FnMut(StatementResult),
) -> (Result<ScriptDone, ScriptError>, Option<PendingRest>) {
    let conn = match pool.acquire_script_conn().await {
        Ok(conn) => conn,
        Err(error) => return (Err(nothing_ran(statements, error)), None),
    };
    let (failure, (ended, pending)) =
        run_statements(conn, pool.dialect(), statements, on_result).await;
    let ran = match &failure {
        Some((statement_index, _)) => &statements[..=*statement_index],
        None => statements,
    };
    (
        sequential_outcome(caps, pool.dialect(), statements, ran, failure, ended),
        pending,
    )
}

/// How a sequential run reports, given how its statements went (`failure`)
/// and how ending its hold on the connection went (`ended`: whether a
/// transaction the script had left open was rolled back — see
/// [`ScriptConn::end`](super::registry::ScriptConn::end)). Over plain data so
/// a test can execute every combination.
///
/// A failed end closed the connection, which ends whatever was open on it
/// server-side, but hubro cannot say whether anything was. After a clean run
/// that is [`ScriptDone::connection_closed`], not an error: every statement
/// did run, and blaming the last one would be false. After a failed statement
/// the statement's error stands, with [`Rollback::ConnectionClosed`] saying
/// what hubro does and doesn't know.
///
/// `schema_changes_kept` is decided over everything that ran, including
/// statements before the script's `BEGIN` and the one that failed — more than
/// the rolled-back transaction held. That can only claim a schema change
/// survived when it was committed anyway or never applied, never the reverse,
/// which is the side [`rollback_cover`] errs on too.
fn sequential_outcome(
    caps: Capabilities,
    dialect: Dialect,
    statements: &[String],
    ran: &[String],
    failure: Option<(usize, DbError)>,
    ended: Result<bool, DbError>,
) -> Result<ScriptDone, ScriptError> {
    let open_transaction = |rolled_back: bool| {
        rolled_back.then(|| OpenTransaction {
            schema_changes_kept: rollback_cover(caps, dialect, ran)
                == Rollback::ExceptSchemaChanges,
        })
    };
    match (failure, ended) {
        (None, Ok(rolled_back)) => Ok(ScriptDone {
            rolled_back: open_transaction(rolled_back),
            connection_closed: false,
        }),
        (None, Err(_)) => Ok(ScriptDone {
            rolled_back: None,
            connection_closed: true,
        }),
        (Some((statement_index, error)), ended) => Err(ScriptError {
            statement_index,
            preview: statement_preview(&statements[statement_index]),
            error,
            rollback: match ended {
                Ok(rolled_back) => match open_transaction(rolled_back) {
                    Some(open) => Rollback::OpenTransaction(open),
                    None => Rollback::None,
                },
                Err(_) => Rollback::ConnectionClosed,
            },
        }),
    }
}

/// The read-only path: each statement through `run`, where the engine refuses
/// a write (see [`ReadOnlyRun`]). Never wrapped in a script-long transaction —
/// a read-only script has nothing to roll back, and on Postgres every
/// statement already runs in a transaction of its own — so a failure reports
/// [`Rollback::None`], as the sequential path does ([`read_only_outcome`]).
async fn run_script_read_only(
    run: ReadOnlyRun,
    dialect: Dialect,
    statements: &[String],
    on_result: impl FnMut(StatementResult),
) -> Result<(), ScriptError> {
    let (failure, ()) = run_statements(run, dialect, statements, on_result).await;
    read_only_outcome(statements, failure)
}

/// How a read-only run reports: a failed statement with nothing rolled back.
fn read_only_outcome(
    statements: &[String],
    failure: Option<(usize, DbError)>,
) -> Result<(), ScriptError> {
    match failure {
        None => Ok(()),
        Some((statement_index, error)) => Err(ScriptError {
            statement_index,
            preview: statement_preview(&statements[statement_index]),
            error,
            rollback: Rollback::None,
        }),
    }
}

/// The atomic path: all statements run in one transaction, committed only if
/// every statement succeeds. Any failure (including a failed commit) rolls the
/// script back, and reports what that rollback covered via [`rollback_cover`]
/// — [`Rollback::Full`] unless the engine lets schema changes escape it.
/// Resolved per failure rather than once up front, since it depends
/// on which statements actually ran.
async fn run_script_atomic(
    pool: &DbPool,
    caps: Capabilities,
    statements: &[String],
    on_result: impl FnMut(StatementResult),
) -> Result<(), ScriptError> {
    let tx = match pool.begin_script_tx().await {
        Ok(tx) => tx,
        // Opening the transaction failed: nothing ran, so nothing rolled back.
        Err(error) => return Err(nothing_ran(statements, error)),
    };
    let (failure, ended) = run_statements(tx, pool.dialect(), statements, on_result).await;
    atomic_outcome(caps, pool.dialect(), statements, failure, ended)
}

/// How an atomic run reports, given how its statements went and how its
/// transaction ended. Over plain data so a test can execute every case.
///
/// A failed statement: its error, with what the rollback covered judged over
/// the statements up to and including it — a schema change later in the
/// script never reached the server. A failed commit: not any one statement's
/// failure, so it is labelled `COMMIT` rather than blaming the last statement,
/// which ran, and every statement is in scope.
fn atomic_outcome(
    caps: Capabilities,
    dialect: Dialect,
    statements: &[String],
    failure: Option<(usize, DbError)>,
    ended: TxEnded,
) -> Result<(), ScriptError> {
    match (failure, ended) {
        (None, TxEnded::Committed(Ok(()))) => Ok(()),
        (None, TxEnded::Committed(Err((error, found)))) => Err(ScriptError {
            statement_index: statements.len().saturating_sub(1),
            preview: "COMMIT".to_string(),
            error,
            rollback: atomic_rollback(caps, dialect, statements, found),
        }),
        (Some((statement_index, error)), TxEnded::RolledBack(found)) => Err(ScriptError {
            statement_index,
            preview: statement_preview(&statements[statement_index]),
            error,
            rollback: atomic_rollback(caps, dialect, &statements[..=statement_index], found),
        }),
        (Some(_), TxEnded::Committed(_)) | (None, TxEnded::RolledBack(_)) => {
            unreachable!("an atomic run commits exactly when no statement failed")
        }
    }
}

/// What a failed atomic run reports its rollback covered, given what
/// [`ScriptTx::rollback`](super::registry::ScriptTx::rollback) found open.
/// Only hubro's own transaction is [`rollback_cover`]'s to judge; a SQL Server
/// batch can commit it from inside (`INSERT … COMMIT`, an `EXEC`'d procedure),
/// and saying "no changes were applied" then hides rows that are in the table.
fn atomic_rollback(
    caps: Capabilities,
    dialect: Dialect,
    ran: &[String],
    found: RolledBack,
) -> Rollback {
    match found {
        RolledBack::Own => rollback_cover(caps, dialect, ran),
        // Committed, then reopened (`… COMMIT BEGIN TRAN`): only the script's
        // transaction was undone, as the sequential path reports one.
        RolledBack::Another => Rollback::OpenTransaction(OpenTransaction {
            schema_changes_kept: rollback_cover(caps, dialect, ran)
                == Rollback::ExceptSchemaChanges,
        }),
        RolledBack::Nothing => Rollback::AlreadyEnded,
        // The connection is gone, and with it whatever was open on it — what
        // `ConnectionClosed` says.
        RolledBack::Unknown => Rollback::ConnectionClosed,
    }
}

/// Whether a script should run atomically in one transaction. Only when the
/// connection has the `transactions` capability at all, only for
/// multi-statement scripts, and only when none of the statements manages its
/// own transaction ([`manages_own_transaction`]) or must run outside one
/// ([`is_non_transactional`]) — wrapping either would error at `BEGIN` or on
/// the offending statement. A non-transactional backend falls back to the
/// sequential autocommit path rather than failing.
pub fn wrap_atomically(caps: Capabilities, dialect: Dialect, statements: &[String]) -> bool {
    // Over the statements a SQL Server batch holds, not the batches: one
    // batch of two INSERTs is two statements that commit apart unless
    // wrapped, and a batch's BEGIN TRAN is the script's own transaction.
    let flattened: Vec<String>;
    let statements = if dialect == Dialect::SqlServer {
        flattened = statements
            .iter()
            .flat_map(|s| split_tsql_statements(s))
            .collect();
        &flattened[..]
    } else {
        statements
    };
    caps.transactions
        && statements.len() > 1
        && !statements.iter().any(|s| {
            // Stripped once per statement: both checks only need the
            // stripped form.
            let stripped = strip_strings_and_comments(s, dialect);
            manages_own_transaction(&stripped) || is_non_transactional(dialect, &stripped)
        })
}

/// Whether an already-stripped statement issues its own transaction
/// control, so the script is managing atomicity itself and must not be
/// wrapped again.
fn manages_own_transaction(stripped: &str) -> bool {
    let words = leading_words(stripped, 3);
    matches!(
        words.first().map(String::as_str),
        Some("begin" | "start" | "commit" | "rollback" | "savepoint" | "release" | "end")
    )
        // CockroachDB: commits the open transaction, so wrapped, the
        // statements before it were committed under a "rolled back" report.
        || words == ["show", "commit", "timestamp"]
}

/// Whether an already-stripped statement is transaction control and nothing
/// else, so it needs no capability of its own.
///
/// Stricter than [`manages_own_transaction`], deliberately: that one decides
/// whether to *decline wrapping*, where a false yes only costs atomicity, so
/// any leading `BEGIN` or `END` will do. Here a false yes waves a statement
/// past the capability gate.
///
/// - **SQL Server**: the whole statement must be `BEGIN|COMMIT|ROLLBACK
///   [TRAN|TRANSACTION [name]]`, `COMMIT|ROLLBACK WORK`, or `BEGIN
///   DISTRIBUTED TRAN|TRANSACTION [name]` — nothing after. T-SQL needs no
///   `;` between statements, so measured, `COMMIT SELECT 5` reports error
///   3902 and runs the SELECT anyway; `BEGIN TRAN EXEC p COMMIT` is three
///   statements. A `BEGIN` that opens a block, `END` (alone a syntax error;
///   `END CONVERSATION` closes a broker conversation), `WITH MARK`, and
///   anything else fall through to "needs both". A name can only be a word
///   or `@variable`; a reserved statement there (`BEGIN TRAN CHECKPOINT`,
///   measured to run the CHECKPOINT) is already charged by
///   [`has_opaque_word`], which runs first.
/// - **Postgres and SQLite**: a statement is one statement, so its leading
///   word speaks for it — except `COMMIT PREPARED` / `ROLLBACK PREPARED`,
///   which finish another session's prepared transaction and so make (or
///   discard) writes this statement never showed.
fn is_bare_transaction_control(dialect: Dialect, stripped: &str) -> bool {
    if !manages_own_transaction(stripped) {
        return false;
    }
    let words = leading_words(stripped, usize::MAX);
    if dialect != Dialect::SqlServer {
        return !words.iter().any(|w| w == "prepared");
    }
    let words: Vec<&str> = words.iter().map(String::as_str).collect();
    matches!(
        words.as_slice(),
        ["begin" | "commit" | "rollback"]
            | ["begin" | "commit" | "rollback", "tran" | "transaction"]
            | ["begin" | "commit" | "rollback", "tran" | "transaction", _]
            | ["commit" | "rollback", "work"]
            | ["begin", "distributed", "tran" | "transaction"]
            | ["begin", "distributed", "tran" | "transaction", _]
    )
}

/// Whether an already-stripped statement can't run (or won't take effect)
/// inside a transaction block, so the script must run sequentially instead
/// of wrapped. `VACUUM`
/// applies to SQLite and Postgres alike (and doesn't exist elsewhere, so the
/// unconditional check is harmless); SQLite value-setting `PRAGMA`s are
/// *silently ignored* in a transaction (worse than erroring — they'd vanish
/// without a trace); the Postgres set errors with "cannot run inside a
/// transaction block"; the SQL Server set covers server-level operations
/// T-SQL refuses inside a user transaction (`CREATE`/`ALTER`/`DROP
/// DATABASE`, `BACKUP`, `RESTORE`, full-text DDL).
fn is_non_transactional(dialect: Dialect, stripped: &str) -> bool {
    let words = leading_words(stripped, 2);
    let first = words.first().map(String::as_str).unwrap_or("");
    let second = words.get(1).map(String::as_str).unwrap_or("");
    if first == "vacuum" {
        return true;
    }
    if dialect == Dialect::Sqlite && first == "pragma" {
        // A value-setting PRAGMA (`= value` or `(value)`) is a no-op inside a
        // transaction; keep the script sequential so it actually applies. This
        // over-declines call-form read PRAGMAs (`PRAGMA table_info(t)`), which
        // `needs_confirmation` lets through, but running those sequentially is
        // harmless.
        if stripped.contains('=') || stripped.contains('(') {
            return true;
        }
    }
    if dialect == Dialect::Postgres {
        if matches!(
            (first, second),
            ("create" | "drop", "database")
                | ("create" | "drop", "tablespace")
                | ("alter", "system")
        ) {
            return true;
        }
        // CREATE/DROP INDEX CONCURRENTLY, REINDEX … CONCURRENTLY.
        if has_word(stripped, |word| word.eq_ignore_ascii_case("concurrently")) {
            return true;
        }
    }
    if dialect == Dialect::SqlServer
        && matches!(
            (first, second),
            ("create" | "alter" | "drop", "database")
                | ("create" | "alter" | "drop", "fulltext")
                | ("backup" | "restore", _)
        )
    {
        return true;
    }
    false
}

/// Whether `sql` is SQL Server transaction control and nothing else (`BEGIN
/// TRAN`, `COMMIT`, `ROLLBACK`, with at most a transaction's name — see
/// [`is_bare_transaction_control`]), which the backend sends as a plain batch:
/// through `sp_executesql` a statement that changes `@@TRANCOUNT` fails with
/// error 266.
///
/// Stricter than the gate's test, because a plain batch is where a `SET`
/// outlives its statement. The gate's name slot takes any word of the
/// stripped text, so `BEGIN TRAN EXEC('SET NOCOUNT ON')` and `BEGIN TRAN
/// SETUSER 'guest'` pass it; here a reserved word in the name's place
/// ([`TSQL_OPAQUE_WORDS`]), a parenthesis, or anything but words and the
/// characters a name is spelled with turns the statement away, and it goes
/// through `sp_executesql` like any other.
pub(crate) fn tsql_is_transaction_control(sql: &str) -> bool {
    let stripped = strip_strings_and_comments(sql, Dialect::SqlServer);
    is_bare_transaction_control(Dialect::SqlServer, &stripped)
        && !has_opaque_word(Dialect::SqlServer, &stripped)
        && stripped
            .chars()
            .all(|c| c.is_alphanumeric() || c.is_whitespace() || matches!(c, '_' | '@' | '#' | ';'))
}

/// How a SQL Server script statement goes to the server — see [`tsql_route`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TsqlRoute {
    /// Through the driver's `sp_executesql` RPC, the usual vehicle. The
    /// server refuses to return from it with `@@TRANCOUNT` changed (error
    /// 266), whatever changed it.
    Rpc,
    /// As a plain batch: a statement `sp_executesql` refuses or fails.
    Batch,
    /// As a plain batch, with the connection's transaction state read before
    /// and after it, and a change failing the statement as 266 would have
    /// (`TransactionChanged` in the SQL Server backend). A plain batch has
    /// no 266 of its own, and this is the guard that replaces it.
    CheckedBatch,
}

/// How a SQL Server script statement is sent — on a connection the run holds
/// and then discards (`held_for_run`), or on a pooled one that goes back to
/// the pool.
///
/// Everywhere: a statement that must lead its batch ([`tsql_leads_own_batch`])
/// and bare transaction control ([`tsql_is_transaction_control`]) are
/// [`TsqlRoute::Batch`], since `sp_executesql` refuses or fails them. On a
/// run's own connection, also a statement that creates a local temp table
/// ([`tsql_creates_temp_table`]), as [`TsqlRoute::CheckedBatch`]:
/// `sp_executesql` is a scope of its own, and a `#temp` table is dropped
/// with the scope that created it, so the script's next statement could not
/// see it. Created by a plain batch, it is the session's, and every later
/// statement sees it from inside its own `sp_executesql` (measured: `INSERT`,
/// `ALTER TABLE`, `CREATE INDEX`). Never on a pooled connection: the table
/// would outlive the statement there and reach whoever borrows it next.
pub(crate) fn tsql_route(sql: &str, held_for_run: bool) -> TsqlRoute {
    let Some(statements) = batch_statements(sql, Dialect::SqlServer) else {
        return tsql_statement_route(sql, held_for_run);
    };
    // A batch of several statements: a plain batch if any one needs it —
    // bare transaction control is the script's own transaction, which a
    // script holding never runs inside hubro's ([`wrap_atomically`] sees the
    // same statement), and a module refuses `sp_executesql`.
    if statements
        .iter()
        .any(|s| tsql_leads_own_batch(s) || tsql_is_transaction_control(s))
    {
        return TsqlRoute::Batch;
    }
    // A checked plain batch when a statement creates a temp table, so the
    // table outlasts the batch as it outlasts the statement alone — and only
    // while nothing else in the batch would carry past it
    // ([`plain_batch_safe`]).
    if held_for_run
        && statements.iter().any(|s| tsql_creates_temp_table(s))
        && statements.iter().all(|s| plain_batch_safe(s))
    {
        return TsqlRoute::CheckedBatch;
    }
    TsqlRoute::Rpc
}

/// [`tsql_route`] for one statement.
fn tsql_statement_route(sql: &str, held_for_run: bool) -> TsqlRoute {
    if tsql_leads_own_batch(sql) || tsql_is_transaction_control(sql) {
        TsqlRoute::Batch
    } else if held_for_run && tsql_creates_temp_table(sql) {
        TsqlRoute::CheckedBatch
    } else {
        TsqlRoute::Rpc
    }
}

/// Whether a statement may share a checked plain batch with a temp table's
/// creator: nothing in it outlasts the batch or reaches the transaction —
/// the `SET`, transaction and opaque words [`tsql_creates_temp_table`] turns
/// a creator away for. Unlike there, a write may: a batch is reported as a
/// whole, and a plain batch reporting no count
/// ([`RowsAffected::Unreported`]) says so rather than stating a wrong one.
///
/// One `SET` is let through where it cannot be a session option: an
/// `UPDATE`'s own clause — the grammar has exactly one — and a variable's
/// assignment (`SET @x = …`), which lasts the batch. A second `SET` is
/// another statement written without a `;` (`UPDATE t SET a = 1 SET NOEXEC
/// ON`), and anything else holding one is turned away. A false "no" costs
/// only the temp table's life past its batch; what guards hubro's
/// transaction is the check after the batch ([`TsqlRoute::CheckedBatch`]).
fn plain_batch_safe(sql: &str) -> bool {
    let stripped = strip_strings_and_comments(sql, Dialect::SqlServer);
    if has_opaque_word(Dialect::SqlServer, &stripped)
        || has_word(&stripped, |word| {
            matches!(
                word.to_ascii_lowercase().as_str(),
                "commit" | "rollback" | "tran" | "transaction"
            )
        })
    {
        return false;
    }
    let sets = stripped
        .split(|c: char| !(c.is_alphanumeric() || c == '_'))
        .filter(|word| word.eq_ignore_ascii_case("set"))
        .count();
    match sets {
        0 => true,
        1 => match leading_words(&stripped, 1).first().map(String::as_str) {
            Some("update") => true,
            Some("set") => stripped.trim_start()["set".len()..]
                .trim_start()
                .starts_with('@'),
            _ => false,
        },
        _ => false,
    }
}

/// Whether a SQL Server statement creates a local temp table — `CREATE
/// TABLE #t`, `SELECT … INTO #t`, the name bare, bracketed or double-quoted
/// — and carries nothing a plain batch would change the meaning of, so
/// [`tsql_route`] may send it as one.
///
/// **What keeps hubro's transaction safe is not this list but the check
/// after the batch** ([`TsqlRoute::CheckedBatch`]). Measured by the review of
/// this route: a creator inside `BEGIN TRY … END TRY BEGIN CATCH … END CATCH`
/// whose DDL a database trigger rolled back (3609) returned normally with
/// the transaction gone, and a later `INSERT` ran in autocommit and persisted
/// under a run reported as failed. No word announces that, and through
/// `sp_executesql` error 266 had stopped it. (A deadlock victim, 1205, is not
/// that shape: measured, it dooms the transaction and the server ends the
/// batch with 3998 on either route. A trigger's rollback leaves nothing
/// doomed, and one that rolls back and reopens kept the count, which only
/// the check's transaction id catches — 266 missed it.)
/// The words here are a first filter, and say what a plain batch would
/// change besides the transaction:
///
/// - **`SET`**: an option set in a plain batch holds for the rest of the
///   session, where through `sp_executesql` it lapses with the call — the
///   property the session's `NOCOUNT OFF` pin rests on (`SESSION_SETUP` in
///   the SQL Server backend). `SET NOEXEC ON` would go on to skip hubro's own
///   `COMMIT`.
/// - **Transaction words** (`BEGIN TRAN`, `COMMIT`, `ROLLBACK`, `SAVE TRAN`,
///   `SET TRANSACTION`): kept on `sp_executesql`, whose own 266 says so first.
/// - **Writes and opaque statements** (`INSERT`, `UPDATE`, `DELETE`,
///   `MERGE`, `TRUNCATE`, [`TSQL_OPAQUE_WORDS`]), and **an `INTO` anywhere
///   but a read's own temp table**: tiberius reports no affected-row counts
///   for a plain batch, so a write in one would report no count
///   ([`RowsAffected::Unreported`]) where it has one. A read (`SELECT … INTO
///   #t`) never reported a count; anything else that
///   fills a table through `INTO` — a permanent one, or a temp table in a
///   statement led by `IF` or `DECLARE` — did.
///
/// The words are read off the stripped text with the gate's own scan, so a
/// word the gate sees, this sees. A false "no" costs only the temp table's
/// life past its statement — what every statement had before.
pub(crate) fn tsql_creates_temp_table(sql: &str) -> bool {
    let stripped = strip_strings_and_comments(sql, Dialect::SqlServer);
    let carries_more = has_opaque_word(Dialect::SqlServer, &stripped)
        || has_word(&stripped, |word| {
            matches!(
                word.to_ascii_lowercase().as_str(),
                "set"
                    | "commit"
                    | "rollback"
                    | "tran"
                    | "transaction"
                    | "insert"
                    | "update"
                    | "delete"
                    | "merge"
                    | "truncate"
            )
        });
    if carries_more {
        return false;
    }
    let read = classify_statement(sql, Dialect::SqlServer).returns_rows();
    let tokens = tsql_name_tokens(sql);
    let mut creates = false;
    for (i, token) in tokens.iter().enumerate() {
        match token {
            NameToken::Word(word) if word == "table" => {
                if i > 0
                    && tokens[i - 1] == NameToken::Word("create".into())
                    && names_temp_table(&tokens[i + 1..])
                {
                    creates = true;
                }
            }
            // Rows written through `INTO` are counted unless the statement
            // is a read, and are only a read's own when they fill its temp
            // table.
            NameToken::Word(word) if word == "into" => {
                if !read || !names_temp_table(&tokens[i + 1..]) {
                    return false;
                }
                creates = true;
            }
            _ => {}
        }
    }
    creates
}

/// A token of a SQL Server statement as [`tsql_creates_temp_table`] reads
/// names.
#[derive(Debug, Clone, PartialEq, Eq)]
enum NameToken {
    /// A bare word: a keyword or a name, lowercased.
    Word(String),
    /// A bracketed or double-quoted name.
    Quoted,
    /// A name, bare or quoted, that starts with `#`: a temp table's.
    Temp,
    /// The `.` between the parts of a name.
    Dot,
    /// Anything else: punctuation, a string literal.
    Other,
}

/// `sql` as [`NameToken`]s, comments skipped.
fn tsql_name_tokens(sql: &str) -> Vec<NameToken> {
    let mut tokens = Vec::new();
    for (range, kind) in lex_regions(sql, Dialect::SqlServer) {
        let text = &sql[range];
        let quoted = || {
            if text[1..].starts_with('#') {
                NameToken::Temp
            } else {
                NameToken::Quoted
            }
        };
        match kind {
            RegionKind::Comment => {}
            RegionKind::Bracket => tokens.push(quoted()),
            RegionKind::String if text.starts_with('"') => tokens.push(quoted()),
            RegionKind::String | RegionKind::DollarQuote => tokens.push(NameToken::Other),
            RegionKind::Code => {
                let bytes = text.as_bytes();
                let mut i = 0;
                while i < bytes.len() {
                    let start = i;
                    if is_word_byte(bytes[i]) {
                        while i < bytes.len() && is_word_byte(bytes[i]) {
                            i += 1;
                        }
                        let word = &text[start..i];
                        tokens.push(if word.starts_with('#') {
                            NameToken::Temp
                        } else {
                            NameToken::Word(word.to_ascii_lowercase())
                        });
                        continue;
                    }
                    match bytes[i] {
                        b'.' => tokens.push(NameToken::Dot),
                        byte if byte.is_ascii_whitespace() => {}
                        _ => tokens.push(NameToken::Other),
                    }
                    i += 1;
                }
            }
        }
    }
    tokens
}

/// Whether the name `tokens` start with is a temp table's: `#t`, or one
/// qualified like `tempdb..#t`, which SQL Server accepts with a warning.
fn names_temp_table(tokens: &[NameToken]) -> bool {
    let mut tokens = tokens.iter();
    loop {
        match tokens.next() {
            Some(NameToken::Temp) => return true,
            Some(NameToken::Dot) => {}
            // A qualifier, which only a dot may follow.
            Some(NameToken::Word(_) | NameToken::Quoted) => {
                if tokens.next() != Some(&NameToken::Dot) {
                    return false;
                }
            }
            _ => return false,
        }
    }
}

/// Whether a row-returning statement that ends a script may have its result
/// cut off at the row cap without being read to the end: it can neither
/// change the session's transaction state nor write anything this module can
/// see. Only then does what the connection said *before* the statement still
/// hold after it, and only then does abandoning the rest of its result lose
/// nothing but rows — see
/// [`ScriptConn::finish_with_read`](super::registry::ScriptConn::finish_with_read).
///
/// "Returns rows" is decided by the first keyword alone, so it says nothing
/// about the rest, and measured, the rest can do anything:
///
/// - **Postgres-wire**: only a statement led by `SELECT`, `WITH`, `VALUES`,
///   `TABLE` or `EXPLAIN`. Not `SHOW`: CockroachDB's `SHOW COMMIT TIMESTAMP`
///   commits the open transaction. Nothing a Postgres function runs can end
///   one (only a procedure can, and `CALL` is not a read).
/// - **SQL Server**: a statement is a whole batch, and one led by `SELECT`
///   can carry `COMMIT` or `BEGIN TRAN` — both measured — so no `BEGIN`,
///   `COMMIT`, `ROLLBACK` or `SAVE` word anywhere in it, no
///   `IMPLICIT_TRANSACTIONS` or `ANSI_DEFAULTS` (which turns it on; under
///   it a `SELECT` opens a transaction),
///   and no opaque word ([`TSQL_OPAQUE_WORDS`]: an `EXEC`'d procedure can
///   change `@@TRANCOUNT`).
/// - **Both**: nothing [`statement_needs`] charges as a write. A
///   data-modifying CTE that returns rows (`WITH w AS (INSERT … RETURNING a)
///   SELECT a FROM w`, T-SQL's `OUTPUT`) writes only as fast as its rows are
///   read. On SQL Server, where a cut-short read's connection is discarded,
///   that is what keeps the write: discarded, it was aborted with the
///   session. On Postgres it changes no data outcome — a cut-short read with
///   nothing open is cancelled only if it has written nothing and otherwise
///   read to the end (`postgres::writable_capped_read`), and one inside an
///   open transaction is rolled back either way — and decides only whether a
///   write that fails after the cap is reported, read to the end before the
///   run ends, or lost in the background read.
/// - **SQLite** never asks: its read is followed by the ordinary end.
///
/// A false "no" costs only the wait for the rest of the result, as every
/// read paid before; a false "yes" reports the wrong transaction state.
pub(crate) fn final_read_may_close(sql: &str, dialect: Dialect) -> bool {
    let needs = statement_needs(sql, dialect);
    if needs.mutate || needs.ddl {
        return false;
    }
    match dialect {
        Dialect::Postgres => matches!(
            first_keyword(sql, dialect).to_ascii_lowercase().as_str(),
            "select" | "with" | "values" | "table" | "explain"
        ),
        Dialect::SqlServer => {
            let stripped = strip_strings_and_comments(sql, dialect);
            !has_opaque_word(dialect, &stripped)
                && !has_word(&stripped, |word| {
                    matches!(
                        word.to_ascii_lowercase().as_str(),
                        "begin"
                            | "commit"
                            | "rollback"
                            | "save"
                            | "implicit_transactions"
                            | "ansi_defaults"
                    )
                })
        }
        Dialect::Sqlite => false,
    }
}

/// Whether SQL Server accepts `sql` only as the first statement of a batch —
/// `CREATE`/`ALTER`/`CREATE OR ALTER` of a procedure, function, trigger or
/// view, and `CREATE SCHEMA`/`DEFAULT`/`RULE`.
///
/// The SQL Server backend asks so it can send such a statement as a plain
/// batch. Its usual vehicle is the driver's `sp_executesql` RPC, which always
/// passes a `@params` argument (empty when there are none), and measured
/// against SQL Server 2022, every statement in this family fails there with
/// error 156, "Incorrect syntax near the keyword" — `PROCEDURE`, `VIEW` and
/// so on — however simple it is. `CREATE TYPE`, `CREATE TABLE` and the rest
/// run there fine, so they keep their affected-row counts.
pub(crate) fn tsql_leads_own_batch(sql: &str) -> bool {
    let stripped = strip_strings_and_comments(sql, Dialect::SqlServer);
    let words = leading_words(&stripped, 4);
    let words: Vec<&str> = words.iter().map(String::as_str).collect();
    let object = match words.as_slice() {
        ["create", "or", "alter", object, ..] => object,
        ["create" | "alter", object, ..] => object,
        _ => return false,
    };
    match *object {
        "proc" | "procedure" | "function" | "trigger" | "view" => true,
        "schema" | "default" | "rule" => words[0] == "create",
        _ => false,
    }
}

/// The first `n` word tokens of an already-stripped statement (lowercased).
/// Working on [`strip_strings_and_comments`] output means a leading comment
/// or quoted text can't masquerade as a keyword.
fn leading_words(stripped: &str, n: usize) -> Vec<String> {
    stripped
        .split(|c: char| !(c.is_alphanumeric() || c == '_'))
        .filter(|word| !word.is_empty())
        .take(n)
        .map(|word| word.to_ascii_lowercase())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_sql_server_read_with_no_result_set_states_no_count() {
        let unreported = StatementOutcome::Affected(RowsAffected::Unreported);
        // `SELECT … INTO`: nothing came back, not even columns.
        let nothing = QueryResult::default();
        assert_eq!(
            read_outcome(Dialect::SqlServer, nothing.clone()),
            unreported
        );
        // A real result set that matched nothing is still zero rows.
        let empty = QueryResult {
            columns: vec![crate::db::ColumnInfo { name: "a".into() }],
            rows: vec![],
        };
        assert_eq!(
            read_outcome(Dialect::SqlServer, empty.clone()),
            StatementOutcome::Rows(empty)
        );
        // Postgres can return a result set without columns (`SELECT FROM t`).
        assert_eq!(
            read_outcome(Dialect::Postgres, nothing.clone()),
            StatementOutcome::Rows(nothing)
        );
    }

    /// [`wrap_atomically`] on a fully capable connection — the only shape
    /// the three current backends have. The capability itself is covered by
    /// `a_non_transactional_connection_never_wraps`.
    fn wraps(dialect: Dialect, statements: &[String]) -> bool {
        wrap_atomically(Capabilities::FULL, dialect, statements)
    }

    /// Splitting is dialect-independent except for SQL Server's GO
    /// handling (exercised separately below); Postgres exercises dollar
    /// quoting.
    fn split(sql: &str) -> Vec<String> {
        split_statements(sql, Dialect::Postgres)
    }

    #[test]
    fn splits_on_semicolons_and_keeps_a_trailing_statement() {
        assert_eq!(
            split("SELECT 1; SELECT 2 ; SELECT 3"),
            ["SELECT 1", "SELECT 2", "SELECT 3"]
        );
    }

    #[test]
    fn a_single_statement_needs_no_semicolon() {
        assert_eq!(split("SELECT 1"), ["SELECT 1"]);
        assert_eq!(split("  SELECT 1;  "), ["SELECT 1"]);
    }

    #[test]
    fn semicolons_inside_quotes_do_not_split() {
        assert_eq!(
            split("SELECT 'a;b'; SELECT \"c;d\""),
            ["SELECT 'a;b'", "SELECT \"c;d\""]
        );
        // Doubled quotes stay inside the string.
        assert_eq!(
            split("SELECT 'it''s;fine'; SELECT 2"),
            ["SELECT 'it''s;fine'", "SELECT 2"]
        );
    }

    #[test]
    fn semicolons_inside_comments_do_not_split() {
        assert_eq!(
            split("SELECT 1 -- trailing; comment\n; SELECT 2"),
            ["SELECT 1 -- trailing; comment", "SELECT 2"]
        );
        assert_eq!(
            split("SELECT /* a;b */ 1; SELECT 2"),
            ["SELECT /* a;b */ 1", "SELECT 2"]
        );
        // Nested block comments (Postgres nests; harmless for SQLite).
        assert_eq!(
            split("SELECT /* x /* y; */ z; */ 1; SELECT 2"),
            ["SELECT /* x /* y; */ z; */ 1", "SELECT 2"]
        );
    }

    #[test]
    fn dollar_quoted_bodies_do_not_split() {
        assert_eq!(
            split("SELECT $$a;b$$; SELECT 2"),
            ["SELECT $$a;b$$", "SELECT 2"]
        );
        assert_eq!(
            split("CREATE FUNCTION f() AS $fn$ BEGIN; END; $fn$; SELECT 2"),
            ["CREATE FUNCTION f() AS $fn$ BEGIN; END; $fn$", "SELECT 2"]
        );
        // A different tag inside the body does not close the quote.
        assert_eq!(
            split("SELECT $a$ x $b$ ; $a$; SELECT 2"),
            ["SELECT $a$ x $b$ ; $a$", "SELECT 2"]
        );
        // $1 is a parameter, not a delimiter.
        assert_eq!(split("SELECT $1; SELECT $2"), ["SELECT $1", "SELECT $2"]);
    }

    #[test]
    fn sqlite_trigger_bodies_do_not_split() {
        let sqlite = |sql| split_statements(sql, Dialect::Sqlite);
        assert_eq!(
            sqlite(
                "CREATE TABLE a (x); \
                 CREATE TRIGGER t AFTER INSERT ON a BEGIN UPDATE b SET n = n + 1; END; \
                 INSERT INTO a VALUES (1)"
            ),
            [
                "CREATE TABLE a (x)",
                "CREATE TRIGGER t AFTER INSERT ON a BEGIN UPDATE b SET n = n + 1; END",
                "INSERT INTO a VALUES (1)",
            ]
        );
        // SQLite's own rule (`sqlite3_complete`): only `; END ;` closes a
        // trigger, so a CASE's `END;` inside the body does not — and neither
        // do several body statements, comments, or lower case.
        assert_eq!(
            sqlite(
                "create temp trigger t after insert on a begin \
                 update b set n = case when n > 5 then 0 else n + 1 end; \
                 insert into b values (1); /* ; */ end /* x */ ; select 2"
            ),
            [
                "create temp trigger t after insert on a begin \
                 update b set n = case when n > 5 then 0 else n + 1 end; \
                 insert into b values (1); /* ; */ end /* x */",
                "select 2",
            ]
        );
        // `sqlite3_complete` also reads a trigger behind EXPLAIN.
        assert_eq!(
            sqlite("EXPLAIN CREATE TRIGGER t AFTER INSERT ON a BEGIN SELECT 1; END; SELECT 2"),
            [
                "EXPLAIN CREATE TRIGGER t AFTER INSERT ON a BEGIN SELECT 1; END",
                "SELECT 2"
            ]
        );
        assert_eq!(
            sqlite("CREATE TEMPORARY TRIGGER t AFTER INSERT ON a BEGIN SELECT 1; END; SELECT 2"),
            [
                "CREATE TEMPORARY TRIGGER t AFTER INSERT ON a BEGIN SELECT 1; END",
                "SELECT 2"
            ]
        );
        // A trailing trigger without its final `;` is kept whole.
        assert_eq!(
            sqlite("CREATE TRIGGER IF NOT EXISTS t AFTER INSERT ON a BEGIN SELECT 1; END"),
            ["CREATE TRIGGER IF NOT EXISTS t AFTER INSERT ON a BEGIN SELECT 1; END"]
        );
    }

    #[test]
    fn bare_transaction_control_still_splits() {
        // `BEGIN` here is a transaction, not a block — it must not swallow
        // the statements after it.
        for dialect in [Dialect::Sqlite, Dialect::Postgres] {
            assert_eq!(
                split_statements("BEGIN; INSERT INTO t VALUES (1); END; SELECT 2", dialect),
                ["BEGIN", "INSERT INTO t VALUES (1)", "END", "SELECT 2"],
                "{dialect:?}"
            );
        }
        for dialect in [Dialect::Sqlite, Dialect::Postgres, Dialect::SqlServer] {
            let sql = "BEGIN TRANSACTION; UPDATE t SET a = 1; COMMIT; SELECT 2";
            assert_eq!(
                // SQL Server's statements: its units are whole batches.
                match dialect {
                    Dialect::SqlServer => split_mssql(sql),
                    _ => split_statements(sql, dialect),
                },
                // SQL Server keeps the terminator it split on.
                {
                    let end = if dialect == Dialect::SqlServer {
                        ";"
                    } else {
                        ""
                    };
                    [
                        format!("BEGIN TRANSACTION{end}"),
                        format!("UPDATE t SET a = 1{end}"),
                        format!("COMMIT{end}"),
                        "SELECT 2".to_string(),
                    ]
                },
                "{dialect:?}"
            );
        }
        assert_eq!(
            split_mssql("BEGIN TRAN; DELETE FROM t; ROLLBACK TRAN"),
            ["BEGIN TRAN;", "DELETE FROM t;", "ROLLBACK TRAN"]
        );
        // T-SQL's other BEGIN statements open no block either.
        assert_eq!(
            split_mssql(
                "BEGIN DISTRIBUTED TRANSACTION; BEGIN DIALOG CONVERSATION @h \
                 FROM SERVICE s TO SERVICE 't'; BEGIN CONVERSATION TIMER (@h) TIMEOUT = 5; \
                 SELECT 2"
            ),
            [
                "BEGIN DISTRIBUTED TRANSACTION;",
                "BEGIN DIALOG CONVERSATION @h FROM SERVICE s TO SERVICE 't';",
                "BEGIN CONVERSATION TIMER (@h) TIMEOUT = 5;",
                "SELECT 2"
            ]
        );
        // A CASE outside any block is closed by its own END and leaves
        // nothing open.
        for dialect in [Dialect::Sqlite, Dialect::Postgres, Dialect::SqlServer] {
            assert_eq!(
                match dialect {
                    Dialect::SqlServer =>
                        split_mssql("SELECT CASE WHEN a THEN 1 END FROM t; SELECT 2"),
                    _ =>
                        split_statements("SELECT CASE WHEN a THEN 1 END FROM t; SELECT 2", dialect),
                },
                [
                    if dialect == Dialect::SqlServer {
                        "SELECT CASE WHEN a THEN 1 END FROM t;"
                    } else {
                        "SELECT CASE WHEN a THEN 1 END FROM t"
                    },
                    "SELECT 2"
                ],
                "{dialect:?}"
            );
        }
    }

    #[test]
    fn sqlserver_module_bodies_run_to_the_end_of_their_batch() {
        // Measured: SQL Server takes everything up to the end of the batch as
        // the body of a procedure, function or trigger — with or without
        // BEGIN … END — so only GO ends one.
        assert_eq!(
            split_mssql("CREATE PROCEDURE p AS BEGIN SELECT 1; SELECT 2; END\nGO\nEXEC p"),
            [
                "CREATE PROCEDURE p AS BEGIN SELECT 1; SELECT 2; END",
                "EXEC p"
            ]
        );
        assert_eq!(
            split_mssql("CREATE OR ALTER PROC p AS SELECT 1; SELECT 2;\nGO\nEXEC p;"),
            ["CREATE OR ALTER PROC p AS SELECT 1; SELECT 2;", "EXEC p;"]
        );
        assert_eq!(
            split_mssql("ALTER TRIGGER tr ON t AFTER INSERT AS BEGIN UPDATE u SET n = n + 1; END;"),
            ["ALTER TRIGGER tr ON t AFTER INSERT AS BEGIN UPDATE u SET n = n + 1; END;"]
        );
        assert_eq!(
            split_mssql(
                "CREATE FUNCTION f() RETURNS int AS BEGIN DECLARE @x int = 1; RETURN @x; END"
            ),
            ["CREATE FUNCTION f() RETURNS int AS BEGIN DECLARE @x int = 1; RETURN @x; END"]
        );
        // A function's body cannot run past its END — measured, a statement
        // after one in the same batch is a server error and nothing is
        // created — but it is still kept with the function, so the server
        // sees what SSMS would send and reports that error itself.
        assert_eq!(
            split_mssql(
                "CREATE FUNCTION f() RETURNS @t TABLE (a int) AS \
                 BEGIN INSERT @t VALUES (1); RETURN; END; SELECT 2"
            ),
            ["CREATE FUNCTION f() RETURNS @t TABLE (a int) AS \
                 BEGIN INSERT @t VALUES (1); RETURN; END; SELECT 2"]
        );
        // Statements before the module in the same batch still split off.
        assert_eq!(
            split_mssql("DROP PROCEDURE IF EXISTS p; CREATE PROCEDURE p AS SELECT 1; SELECT 2"),
            [
                "DROP PROCEDURE IF EXISTS p;",
                "CREATE PROCEDURE p AS SELECT 1; SELECT 2"
            ]
        );
    }

    #[test]
    fn sqlserver_blocks_do_not_split_inside() {
        assert_eq!(
            split_mssql(
                "IF 1 = 1 BEGIN SELECT 1; SELECT 2; END ELSE BEGIN SELECT 3; END; SELECT 4"
            ),
            [
                "IF 1 = 1 BEGIN SELECT 1; SELECT 2; END ELSE BEGIN SELECT 3; END;",
                "SELECT 4"
            ]
        );
        assert_eq!(
            split_mssql(
                "BEGIN TRY SELECT 1/0; END TRY BEGIN CATCH SELECT \
                 CASE WHEN 1 = 1 THEN 'x' END; END CATCH; SELECT 2"
            ),
            [
                "BEGIN TRY SELECT 1/0; END TRY BEGIN CATCH SELECT \
                 CASE WHEN 1 = 1 THEN 'x' END; END CATCH;",
                "SELECT 2"
            ]
        );
        // A transaction opened inside a block is not a block of its own, and
        // `END CONVERSATION` is a statement, not the close of one.
        assert_eq!(
            split_mssql(
                "BEGIN BEGIN TRAN; SELECT 1; COMMIT; END; SELECT 2; \
                 WHILE 1 = 0 BEGIN END CONVERSATION @h; SELECT 3; END; SELECT 4"
            ),
            [
                "BEGIN BEGIN TRAN; SELECT 1; COMMIT; END;",
                "SELECT 2;",
                "WHILE 1 = 0 BEGIN END CONVERSATION @h; SELECT 3; END;",
                "SELECT 4"
            ]
        );
        // Variables, bracketed names and strings named like keywords count
        // for nothing.
        assert_eq!(
            split_mssql("SELECT @begin, [begin], 'begin', #begin; SELECT 2"),
            ["SELECT @begin, [begin], 'begin', #begin;", "SELECT 2"]
        );
        // A keyword spelled at the end of a longer identifier is not one
        // (measured: `Ébegin` is a legal name on SQL Server).
        assert_eq!(
            split_mssql("SELECT 1 AS Ébegin; SELECT 2"),
            ["SELECT 1 AS Ébegin;", "SELECT 2"]
        );
        // SQL Server takes Unicode whitespace as whitespace (measured with
        // U+00A0 and U+3000), so it separates keywords like a space does.
        assert_eq!(
            split_mssql(
                "IF 1 = 1 BEGIN\u{a0}SELECT 1;\u{3000}END\u{a0}; \
                 BEGIN\u{a0}TRAN; SELECT 2"
            ),
            [
                "IF 1 = 1 BEGIN\u{a0}SELECT 1;\u{3000}END\u{a0};",
                "BEGIN\u{a0}TRAN;",
                "SELECT 2"
            ]
        );
        // A number ends at its first non-digit (non-hex after `0x`), so the
        // word after it is a keyword (measured: `0xAUPDATE` ran an UPDATE)…
        assert_eq!(
            split_mssql("SELECT 1BEGIN SELECT 1; END; SELECT 2"),
            ["SELECT 1BEGIN SELECT 1; END;", "SELECT 2"]
        );
        // A hex literal takes every hex letter, so `0x1BEGIN` is `0x1BE`, then
        // `GIN` — no BEGIN. (BEGIN, CASE and END all start with a hex letter.)
        assert_eq!(
            split_mssql("SELECT 0x1BEGIN; SELECT 2"),
            ["SELECT 0x1BEGIN;", "SELECT 2"]
        );
        // …but a digit inside a variable, temp table or other name starts no
        // number, so nothing in it is a keyword (measured: `@1begin` and
        // `#1begin` are legal names).
        assert_eq!(
            split_mssql("SELECT @1begin, #1begin, x1begin, x$1begin; SELECT 2"),
            ["SELECT @1begin, #1begin, x1begin, x$1begin;", "SELECT 2"]
        );
        // An unclosed block cannot swallow the next batch: GO ends it.
        assert_eq!(
            split_mssql("BEGIN SELECT 1;\nGO\nSELECT 2; SELECT 3"),
            ["BEGIN SELECT 1;", "SELECT 2;", "SELECT 3"]
        );
    }

    #[test]
    fn postgres_sql_standard_bodies_do_not_split() {
        // `BEGIN ATOMIC … END` (Postgres 14+) is not dollar-quoted. Inside
        // CREATE [OR REPLACE] FUNCTION/PROCEDURE, BEGIN ATOMIC and (inside a
        // block) CASE open, END closes.
        assert_eq!(
            split(
                "CREATE OR REPLACE FUNCTION f() RETURNS int LANGUAGE sql BEGIN ATOMIC \
                 SELECT 1; SELECT CASE WHEN true THEN 2 END; SELECT (CASE WHEN true THEN 3 END); \
                 END; SELECT f()"
            ),
            [
                "CREATE OR REPLACE FUNCTION f() RETURNS int LANGUAGE sql BEGIN ATOMIC \
                 SELECT 1; SELECT CASE WHEN true THEN 2 END; SELECT (CASE WHEN true THEN 3 END); \
                 END",
                "SELECT f()"
            ]
        );
        assert_eq!(
            split(
                "CREATE PROCEDURE p() LANGUAGE sql BEGIN ATOMIC \
                 INSERT INTO t VALUES (1); INSERT INTO t VALUES (2); END; CALL p()"
            ),
            [
                "CREATE PROCEDURE p() LANGUAGE sql BEGIN ATOMIC \
                 INSERT INTO t VALUES (1); INSERT INTO t VALUES (2); END",
                "CALL p()"
            ]
        );
        // `begin` is unreserved in Postgres: a parameter or a qualified column
        // of that name opens nothing — only `BEGIN ATOMIC` does, which is
        // stricter than psql (it counts any BEGIN outside parentheses, and
        // would merge these two).
        assert_eq!(
            split(
                "CREATE FUNCTION f(begin int) RETURNS int LANGUAGE sql RETURN begin + 1; \
                 SELECT 2"
            ),
            [
                "CREATE FUNCTION f(begin int) RETURNS int LANGUAGE sql RETURN begin + 1",
                "SELECT 2"
            ]
        );
        assert_eq!(
            split(
                "CREATE FUNCTION f() RETURNS int LANGUAGE sql BEGIN ATOMIC \
                 SELECT t.end FROM t; SELECT 1; END; SELECT 2"
            ),
            [
                "CREATE FUNCTION f() RETURNS int LANGUAGE sql BEGIN ATOMIC \
                 SELECT t.end FROM t; SELECT 1; END",
                "SELECT 2"
            ]
        );
        // A label inside parentheses is a label too (measured legal).
        assert_eq!(
            split(
                "CREATE FUNCTION f() RETURNS int LANGUAGE sql BEGIN ATOMIC \
                 SELECT (SELECT 1 AS end); END; SELECT 2"
            ),
            [
                "CREATE FUNCTION f() RETURNS int LANGUAGE sql BEGIN ATOMIC \
                 SELECT (SELECT 1 AS end); END",
                "SELECT 2"
            ]
        );
        // Postgres takes any keyword as a column label after AS (measured
        // with both of these), so a label counts for nothing: `AS case` would
        // otherwise never close, and `AS end` would close early.
        assert_eq!(
            split(
                "CREATE FUNCTION f() RETURNS int LANGUAGE sql BEGIN ATOMIC \
                 SELECT 1 AS case; END; SELECT 2"
            ),
            [
                "CREATE FUNCTION f() RETURNS int LANGUAGE sql BEGIN ATOMIC \
                 SELECT 1 AS case; END",
                "SELECT 2"
            ]
        );
        assert_eq!(
            split(
                "CREATE FUNCTION f() RETURNS int LANGUAGE sql BEGIN ATOMIC \
                 SELECT 1 AS end; SELECT 2; END; SELECT 3"
            ),
            [
                "CREATE FUNCTION f() RETURNS int LANGUAGE sql BEGIN ATOMIC \
                 SELECT 1 AS end; SELECT 2; END",
                "SELECT 3"
            ]
        );
        // An implicit alias named `begin` (legal: measured) opens nothing
        // either, since only `BEGIN ATOMIC` does.
        assert_eq!(
            split(
                "CREATE FUNCTION f() RETURNS int LANGUAGE sql BEGIN ATOMIC \
                 SELECT 1 begin; END; SELECT 2"
            ),
            [
                "CREATE FUNCTION f() RETURNS int LANGUAGE sql BEGIN ATOMIC \
                 SELECT 1 begin; END",
                "SELECT 2"
            ]
        );
        // Outside a routine, BEGIN is a transaction (see
        // `bare_transaction_control_still_splits`) and PL/pgSQL bodies are
        // dollar-quoted, so nothing else is tracked.
    }

    /// The whole wire from script text to the refusal: split, charge each
    /// statement, refuse under each capability set. On SQL Server one
    /// statement here can be several on the server (T-SQL needs no `;`
    /// between them), so nothing may be charged by its first word alone, nor
    /// for only the half of it a keyword names.
    #[test]
    fn a_script_is_charged_for_everything_it_runs() {
        const NONE: (bool, bool) = (false, false);
        const MUTATE: (bool, bool) = (true, false);
        const DDL: (bool, bool) = (false, true);
        const BOTH: (bool, bool) = (true, true);
        use Dialect::{Postgres as Pg, SqlServer as Ms, Sqlite as Lite};
        let cases: &[(Dialect, &str, (bool, bool))] = &[
            // Transaction words followed by more than transaction control.
            (Ms, "BEGIN TRAN EXEC sp_rename 'a', 'b' COMMIT", BOTH),
            (Ms, "BEGIN TRANSACTION t1 EXEC('DROP TABLE t') COMMIT", BOTH),
            (Ms, "COMMIT EXEC sp_rename 'a', 'b'", BOTH),
            (Ms, "ROLLBACK EXEC p", BOTH),
            (
                Ms,
                "BEGIN TRAN BEGIN EXEC sp_rename 'a', 'b'; END COMMIT",
                BOTH,
            ),
            (Ms, "BEGIN TRAN CHECKPOINT", BOTH),
            (Ms, "BEGIN TRAN t1 WITH MARK 'm'", BOTH),
            // A merged block is charged for everything in it.
            (
                Ms,
                "BEGIN DELETE FROM a; EXEC sp_rename 'x', 'y'; END",
                BOTH,
            ),
            (
                Ms,
                "IF 1 = 1 BEGIN DELETE FROM a; EXEC('DROP TABLE x'); END",
                BOTH,
            ),
            (Ms, "BEGIN CREATE TABLE t (a int); EXEC dml_proc; END", BOTH),
            (Ms, "BEGIN EXEC sp_rename 'a', 'b'; END", BOTH),
            (Ms, "BEGIN EXEC sp_rename 'a', 'b' END", BOTH),
            (
                Ms,
                "BEGIN TRY EXEC do_it; END TRY BEGIN CATCH END CATCH",
                BOTH,
            ),
            (Ms, "INSERT INTO t EXEC p", BOTH),
            // A definition whose body calls something: over-charged, declared.
            (Ms, "CREATE PROCEDURE p AS EXEC q", BOTH),
            // Reads followed by something else.
            (Ms, "SELECT 1 EXEC sp_rename 'a', 'b'", BOTH),
            (Ms, "SELECT 1 EXEC('DELETE FROM t')", BOTH),
            (Ms, "SELECT 1 DELETE FROM t", MUTATE),
            (Ms, "SELECT 1 BEGIN DELETE FROM t; END", MUTATE),
            (Ms, "SELECT 1 CREATE TABLE x (a int)", DDL),
            (Ms, "SELECT 1 DISABLE TRIGGER tr ON t", DDL),
            (Ms, "SELECT 1 DENY SELECT ON t TO u", DDL),
            (Ms, "SELECT 1 RECEIVE * FROM q", BOTH),
            (Ms, "SELECT 1 SEND ON CONVERSATION @h", BOTH),
            (
                Ms,
                "SELECT 1 BEGIN DIALOG @h FROM SERVICE s TO SERVICE 't'",
                BOTH,
            ),
            // Service Broker: exempt as "transaction control" before, now not.
            (Ms, "END CONVERSATION @h", BOTH),
            (
                Ms,
                "BEGIN DIALOG CONVERSATION @h FROM SERVICE s TO SERVICE 't'",
                BOTH,
            ),
            (Ms, "BEGIN DIALOG @h FROM SERVICE s TO SERVICE 't'", BOTH),
            (Ms, "BEGIN CONVERSATION TIMER (@h) TIMEOUT = 5", BOTH),
            // Postgres: finishing another session's prepared transaction.
            (Pg, "COMMIT PREPARED 'x'", BOTH),
            (Pg, "ROLLBACK PREPARED 'x'", BOTH),
            // SQLite's REPLACE statement, wherever it stands; only the
            // function call `replace(` is a read.
            (Lite, "REPLACE INTO t VALUES (1)", MUTATE),
            (
                Lite,
                "WITH x AS (SELECT 1 AS a) REPLACE INTO t SELECT a FROM x",
                MUTATE,
            ),
            (
                Lite,
                "WITH x AS (SELECT replace(a, 'b', 'c') AS a FROM s) REPLACE INTO t SELECT a FROM x",
                MUTATE,
            ),
            (
                Pg,
                "WITH x AS (SELECT replace(a, 'b', 'c') AS a FROM s) SELECT * FROM x",
                NONE,
            ),
            // A setting pragma's call form sets, a reader's reads.
            (Lite, "PRAGMA user_version(5)", BOTH),
            (Lite, "PRAGMA table_info(t)", NONE),
            // SQLite pragmas that write with no argument.
            (Lite, "PRAGMA optimize", BOTH),
            (Lite, "PRAGMA incremental_vacuum", BOTH),
            (Lite, "PRAGMA main.wal_checkpoint", BOTH),
            // Pass-through: the remote text is a string another server runs.
            (Ms, "SELECT * FROM OPENQUERY(lnk, 'DELETE FROM t')", BOTH),
            (
                Ms,
                "SELECT * FROM OPENROWSET('SQLNCLI', 'srv', 'EXEC p')",
                BOTH,
            ),
            (
                Ms,
                "SELECT * FROM OPENDATASOURCE('SQLNCLI', 'x').db.dbo.t",
                BOTH,
            ),
            (Ms, "ADD SIGNATURE TO p BY CERTIFICATE c", DDL),
            (
                Ms,
                "SELECT 1 ADD COUNTER SIGNATURE TO p BY CERTIFICATE c",
                DDL,
            ),
            // Lexing that disagrees with the engine hides a statement: each
            // of these was a read needing nothing (see `region_at`).
            (
                Lite,
                "SELECT 1 AS `'`; DELETE FROM t; SELECT 1 AS `'`",
                MUTATE,
            ),
            (
                Lite,
                "SELECT 1 AS [']; DELETE FROM t; SELECT 1 AS [']",
                MUTATE,
            ),
            (Lite, "SELECT 1 /* /* */ ; DELETE FROM t; /* */", MUTATE),
            (
                Lite,
                "SELECT 1 AS x$y$; DELETE FROM t; SELECT 1 AS x$y$",
                MUTATE,
            ),
            (Ms, "SELECT 1 --c\rDELETE FROM t", MUTATE),
            (
                Ms,
                "SELECT 1 AS x$y$ DELETE FROM t SELECT 1 AS x$y$",
                MUTATE,
            ),
            (
                Pg,
                "WITH d AS (SELECT 1) --c\r, e AS (DELETE FROM t RETURNING 1) SELECT 1",
                MUTATE,
            ),
            (
                Pg,
                "--c\rWITH e AS (DELETE FROM t RETURNING 1)\nSELECT 1",
                MUTATE,
            ),
            (
                Pg,
                "WITH d AS (SELECT E'\\'' AS a), e AS (DELETE FROM t RETURNING 1) SELECT 1 --'",
                MUTATE,
            ),
            (
                Pg,
                "WITH d AS (SELECT E'a'\n'\\'' AS a), e AS (DELETE FROM t RETURNING 1) \
                 SELECT 1 --'",
                MUTATE,
            ),
            (
                Pg,
                "WITH d AS (SELECT 1 AS x$y$), e AS (DELETE FROM t RETURNING 1) SELECT 1 AS x$y$",
                MUTATE,
            ),
            (
                Pg,
                "WITH d AS (SELECT $é$'$é$ AS a), e AS (DELETE FROM t RETURNING 1) SELECT 1 --'",
                MUTATE,
            ),
            // T-SQL ends a number at the first letter (measured: each ran).
            (Ms, "SELECT 1DELETE FROM t", MUTATE),
            (Ms, "SELECT .5DELETE FROM t", MUTATE),
            (Ms, "SELECT 1e+1DELETE FROM t", MUTATE),
            (Ms, "SELECT $1DELETE FROM t", MUTATE),
            (Ms, "SELECT €1DELETE FROM t", MUTATE),
            (Ms, "SELECT 0x1UPDATE t SET a = 5", MUTATE),
            (Ms, "SELECT 1BEGIN DELETE FROM t; SELECT 2; END", MUTATE),
            (Ms, "SELECT 0xAUPDATE t SET a = 1", MUTATE),
            (Ms, "SELECT 0x1FINSERT INTO t VALUES (1)", MUTATE),
            (
                Ms,
                "SELECT 0xFMERGE INTO t USING u ON 1 = 1 WHEN MATCHED THEN DELETE;",
                MUTATE,
            ),
            (Ms, "SELECT 0xAGRANT SELECT ON t TO u", DDL),
            (Ms, "SELECT 0xFREVOKE SELECT ON t FROM u", DDL),
            (Ms, "SELECT 0xAKILL 5", BOTH),
            (Ms, "SELECT 0xFINTO t2 FROM t", BOTH),
            (Ms, "SELECT ＄1DELETE FROM t", MUTATE),
            (Ms, "SELECT 1e1EXEC sp_who", BOTH),
            (Ms, "SELECT.5DELETE FROM t", MUTATE),
            (Ms, "SELECT DISTINCT.5DELETE FROM t", MUTATE),
            (Ms, "SELECT $.5DELETE FROM t", MUTATE),
            (Ms, "SELECT 1E+EXEC p", BOTH),
            (Ms, "SELECT 1EXECUTE p", BOTH),
            (Ms, "SELECT 1EXEC sp_rename 'a', 'b'", BOTH),
            // Allowed: transaction control alone, and plain reads.
            (Ms, "BEGIN TRAN", NONE),
            (Ms, "BEGIN TRANSACTION", NONE),
            (Ms, "BEGIN DISTRIBUTED TRANSACTION", NONE),
            (Ms, "COMMIT", NONE),
            (Ms, "COMMIT TRAN", NONE),
            (Ms, "COMMIT WORK", NONE),
            (Ms, "ROLLBACK TRANSACTION t1", NONE),
            (Ms, "ROLLBACK TRAN @t", NONE),
            (Ms, "SELECT 1", NONE),
            (Ms, "SELECT REPLACE(name, 'a', 'b') FROM t", NONE),
            (Ms, "WITH x AS (SELECT 1 AS a) SELECT * FROM x", NONE),
            (Pg, "COMMIT", NONE),
            (Pg, "ROLLBACK TO SAVEPOINT s", NONE),
            (Pg, "SELECT 1", NONE),
            (Lite, "BEGIN", NONE),
            (Lite, "PRAGMA integrity_check", NONE),
            // ...and quoted text that only looks like a write.
            (Lite, "SELECT 1 AS `x;DELETE`", NONE),
            (Lite, "SELECT 1 AS [x;DELETE]", NONE),
            (Pg, "SELECT E'it''s \\' DELETE'", NONE),
            (Pg, "SELECT $q$ DELETE $q$", NONE),
            (Ms, "SELECT 1 /* /* */ DELETE FROM t */", NONE),
            (Ms, "SELECT 0x1F, 1e5, $2.50 FROM t", NONE),
            // A hex literal keeps its hex letters (`0xFDELETE` is `0x0FDE`,
            // then `LETE`: measured), and names keep their digits.
            (Ms, "SELECT 0xFDELETE FROM t", NONE),
            (
                Ms,
                "SELECT @1delete, #1delete, x1delete, x$1delete, É1delete",
                NONE,
            ),
        ];
        let caps_sets = [
            ("full", Capabilities::FULL),
            ("read-only", Capabilities::FULL.read_only()),
            (
                "mutate without ddl",
                Capabilities {
                    ddl: false,
                    ..Capabilities::FULL
                },
            ),
            (
                "ddl without mutate",
                Capabilities {
                    mutate: false,
                    ..Capabilities::FULL
                },
            ),
        ];
        // Every opaque word charges both after a read, one by one.
        let opaque: Vec<(Dialect, String, (bool, bool))> = TSQL_OPAQUE_WORDS
            .iter()
            .map(|word| (Ms, format!("SELECT 1 {word} x"), BOTH))
            .collect();
        let cases = cases
            .iter()
            .map(|&(dialect, sql, needs)| (dialect, sql, needs))
            .chain(opaque.iter().map(|(d, sql, n)| (*d, sql.as_str(), *n)));
        // SQL Server statements keep the `;` they were split on, so each case
        // there runs again with one: a terminator must never change a charge.
        let cases = cases.flat_map(|(dialect, sql, needs)| {
            let terminated = (dialect == Ms).then(|| format!("{sql};"));
            std::iter::once((dialect, sql.to_string(), needs))
                .chain(terminated.map(|sql| (dialect, sql, needs)))
        });
        for (dialect, sql, (mutate, ddl)) in cases {
            let sql = sql.as_str();
            let statements = split_statements(sql, dialect);
            if dialect == Ms && sql.ends_with(';') {
                assert!(
                    statements.last().is_some_and(|s| s.ends_with(';')),
                    "{sql:?} should reach the gate with its terminator, split as {statements:?}"
                );
            }
            let charged = statements.iter().map(|s| statement_needs(s, dialect)).fold(
                StatementNeeds::default(),
                |a, b| StatementNeeds {
                    mutate: a.mutate || b.mutate,
                    ddl: a.ddl || b.ddl,
                },
            );
            assert_eq!(
                charged,
                StatementNeeds { mutate, ddl },
                "{sql:?} on {dialect:?}, split as {statements:?}"
            );
            for (name, caps) in caps_sets {
                let refused = (mutate && !caps.mutate) || (ddl && !caps.ddl);
                assert_eq!(
                    script_refusal(caps, &statements, dialect).is_some(),
                    refused,
                    "{sql:?} on {dialect:?} under {name}"
                );
            }
        }
        // A NUL is refused whatever the capabilities: measured, sqlx-sqlite
        // spins forever on one and SQL Server runs straight past it.
        for dialect in [Lite, Pg, Ms] {
            let statements = split_statements("SELECT 1;\0DELETE FROM t", dialect);
            // SQL Server sends the script as one batch, so the batch is
            // refused; elsewhere, the second statement.
            let at = if dialect == Ms { 0 } else { 1 };
            for (name, caps) in caps_sets {
                assert_eq!(
                    script_refusal(caps, &statements, dialect),
                    Some((at, NUL_IN_SCRIPT)),
                    "{dialect:?} under {name}"
                );
            }
        }
    }

    #[test]
    fn unterminated_quotes_swallow_the_rest() {
        assert_eq!(split("SELECT 'a; b"), ["SELECT 'a; b"]);
        assert_eq!(split("SELECT $$a; b"), ["SELECT $$a; b"]);
    }

    #[test]
    fn empty_and_comment_only_statements_are_skipped() {
        assert_eq!(split(""), Vec::<String>::new());
        assert_eq!(split(" ;  ; ;"), Vec::<String>::new());
        assert_eq!(
            split("-- just a comment\n; /* and another */;"),
            Vec::<String>::new()
        );
        assert_eq!(split(";;SELECT 1;; -- done\n;"), ["SELECT 1"]);
    }

    #[test]
    fn multibyte_content_splits_cleanly() {
        assert_eq!(
            split("SELECT 'смузи;ярлык'; SELECT 'ünïcödé'"),
            ["SELECT 'смузи;ярлык'", "SELECT 'ünïcödé'"]
        );
    }

    #[test]
    fn a_sql_server_batch_is_decided_over_its_statements() {
        let ms = Dialect::SqlServer;
        let needs = |sql| statement_needs(sql, ms);
        // The gate charges a batch for any of its statements.
        assert!(needs("SELECT 1; DELETE FROM t").mutate);
        assert!(needs("SELECT 1; DROP TABLE t").ddl);
        assert_eq!(needs("SELECT 1; SELECT 2"), StatementNeeds::default());
        assert!(needs_confirmation("SELECT 1; UPDATE t SET a = 1", ms));
        assert!(!needs_confirmation("SELECT 1; SELECT 2", ms));

        // A batch is read when any statement returns rows: a variable declared
        // then read is a read.
        assert!(returns_rows("DECLARE @x int = 1; SELECT @x", ms));
        assert!(!returns_rows(
            "DECLARE @x int = 1; INSERT INTO t VALUES (@x)",
            ms
        ));

        // Wrapping counts statements, not batches.
        let wraps = |batches: &[&str]| {
            wrap_atomically(
                Capabilities::FULL,
                ms,
                &batches.iter().map(|b| b.to_string()).collect::<Vec<_>>(),
            )
        };
        assert!(wraps(&[
            "INSERT INTO t VALUES (1); INSERT INTO t VALUES (2)"
        ]));
        assert!(!wraps(&["INSERT INTO t VALUES (1)"]));
        assert!(!wraps(&["BEGIN TRAN; INSERT INTO t VALUES (1); COMMIT"]));
        assert!(!wraps(&[
            "INSERT INTO t VALUES (1); BACKUP DATABASE db TO DISK = 'x'"
        ]));

        // How a batch is sent: a plain batch when any statement needs one, a
        // checked one for a temp table among plain-batch-safe statements —
        // writes included — on a run's own connection, `sp_executesql`
        // otherwise.
        let routes = |sql: &str| (tsql_route(sql, true), tsql_route(sql, false));
        use TsqlRoute::{Batch, CheckedBatch, Rpc};
        for (sql, want) in [
            ("BEGIN TRAN; INSERT INTO t VALUES (1)", (Batch, Batch)),
            ("INSERT INTO t VALUES (1); COMMIT", (Batch, Batch)),
            (
                "CREATE TABLE #t (a int); INSERT INTO #t VALUES (1)",
                (CheckedBatch, Rpc),
            ),
            (
                "SELECT 1 AS a INTO #s; UPDATE #s SET a = 2; SELECT a FROM #s",
                (CheckedBatch, Rpc),
            ),
            ("CREATE TABLE #t (a int); SET NOCOUNT ON", (Rpc, Rpc)),
            // An UPDATE's own SET, and a variable's, carry nothing past the
            // batch; a second SET is a session option hiding behind one.
            (
                "CREATE TABLE #t (a int); UPDATE #t SET a = 1",
                (CheckedBatch, Rpc),
            ),
            (
                "DECLARE @x int; SET @x = 1; CREATE TABLE #t (a int)",
                (CheckedBatch, Rpc),
            ),
            (
                "CREATE TABLE #t (a int); UPDATE #t SET a = 1 SET NOEXEC ON",
                (Rpc, Rpc),
            ),
            (
                "CREATE TABLE #t (a int); SET @x = 1 SET NOEXEC ON",
                (Rpc, Rpc),
            ),
            ("CREATE TABLE #t (a int); SET XACT_ABORT ON", (Rpc, Rpc)),
            (
                "CREATE TABLE #t (a int); ALTER TABLE #t ADD CONSTRAINT c DEFAULT 0 FOR a",
                (CheckedBatch, Rpc),
            ),
            ("CREATE TABLE #t (a int); EXEC p", (Rpc, Rpc)),
            ("CREATE TABLE #t (a int); SAVE TRANSACTION s", (Rpc, Rpc)),
            (
                "INSERT INTO t VALUES (1); INSERT INTO t VALUES (2)",
                (Rpc, Rpc),
            ),
            ("DECLARE @x int = 1; SELECT @x", (Rpc, Rpc)),
        ] {
            assert_eq!(routes(sql), want, "{sql:?}");
        }
    }

    #[test]
    fn a_sql_server_script_is_sent_as_its_go_batches() {
        let ms = |sql| split_statements(sql, Dialect::SqlServer);
        // A batch is everything between GO lines, `;`s and all — what SSMS
        // sends, and what keeps a DECLAREd variable in scope.
        assert_eq!(
            ms("DECLARE @x int = 1;\nINSERT INTO t VALUES (@x);\nGO\nSELECT 2;\nSELECT 3"),
            [
                "DECLARE @x int = 1;\nINSERT INTO t VALUES (@x);",
                "SELECT 2;\nSELECT 3"
            ]
        );
        // A GO only counts alone on its line, as before.
        assert_eq!(
            ms("SELECT 'GO'; SELECT 1 -- GO"),
            ["SELECT 'GO'; SELECT 1 -- GO"]
        );
        // Empty batches leave nothing behind.
        assert_eq!(ms("GO\n;\nGO\nSELECT 1\nGO"), ["SELECT 1"]);
        // Every other dialect still splits on `;`.
        assert_eq!(
            split_statements("SELECT 1; SELECT 2", Dialect::Postgres),
            ["SELECT 1", "SELECT 2"]
        );
    }

    /// A SQL Server script's statements, `;` and `GO` both splitting — the
    /// split every per-statement decision over a batch folds from.
    fn split_mssql(sql: &str) -> Vec<String> {
        split_tsql_statements(sql)
    }

    #[test]
    fn go_lines_split_batches_on_sqlserver() {
        assert_eq!(
            split_mssql("SELECT 1\nGO\nSELECT 2"),
            ["SELECT 1", "SELECT 2"]
        );
        // Mixed case and surrounding whitespace are fine.
        assert_eq!(
            split_mssql("SELECT 1\n  go  \nSELECT 2"),
            ["SELECT 1", "SELECT 2"]
        );
        // Windows line endings.
        assert_eq!(
            split_mssql("SELECT 1\r\nGO\r\nSELECT 2"),
            ["SELECT 1", "SELECT 2"]
        );
        // A trailing GO leaves no empty statement behind.
        assert_eq!(split_mssql("SELECT 1\nGO"), ["SELECT 1"]);
        assert_eq!(split_mssql("SELECT 1\nGO\n"), ["SELECT 1"]);
        // GO after a semicolon-terminated statement adds nothing.
        assert_eq!(
            split_mssql("SELECT 1;\nGO\nSELECT 2"),
            ["SELECT 1;", "SELECT 2"]
        );
        // Consecutive GO lines produce no empty statements.
        assert_eq!(split_mssql("GO\nGO\nSELECT 1\nGO\nGO"), ["SELECT 1"]);
    }

    #[test]
    fn go_with_a_repeat_count_is_a_plain_separator() {
        // `GO 5` (run the batch five times in sqlcmd/SSMS) is treated as a
        // plain separator; the count is deliberately ignored.
        assert_eq!(
            split_mssql("SELECT 1\nGO 5\nSELECT 2"),
            ["SELECT 1", "SELECT 2"]
        );
        assert_eq!(split_mssql("SELECT 1\ngo 12  "), ["SELECT 1"]);
    }

    #[test]
    fn go_not_alone_on_its_line_does_not_split() {
        // Inside a string literal (even across lines).
        assert_eq!(split_mssql("SELECT 'a\nGO\nb'"), ["SELECT 'a\nGO\nb'"]);
        assert_eq!(split_mssql("SELECT 'GO'"), ["SELECT 'GO'"]);
        // Inside comments.
        assert_eq!(split_mssql("SELECT 1 -- GO\n+ 2"), ["SELECT 1 -- GO\n+ 2"]);
        assert_eq!(
            split_mssql("SELECT 1 /*\nGO\n*/ + 2"),
            ["SELECT 1 /*\nGO\n*/ + 2"]
        );
        // Sharing a line with other code (GO is not a T-SQL keyword).
        assert_eq!(split_mssql("SELECT 1 GO"), ["SELECT 1 GO"]);
        // Identifiers that merely start with GO.
        assert_eq!(split_mssql("SELECT 1\nGOTO x"), ["SELECT 1\nGOTO x"]);
        assert_eq!(split_mssql("GO2 x"), ["GO2 x"]);
        // A GO line right after a multi-line string still splits: the code
        // newline after the closing quote resets line tracking.
        assert_eq!(
            split_mssql("SELECT 'a\nb'\nGO\nSELECT 2"),
            ["SELECT 'a\nb'", "SELECT 2"]
        );
    }

    #[test]
    fn bracketed_identifiers_do_not_split_on_sqlserver() {
        // A `;` inside brackets is identifier text, not a separator.
        assert_eq!(
            split_mssql("SELECT [a;b] FROM t; SELECT 2"),
            ["SELECT [a;b] FROM t;", "SELECT 2"]
        );
        // `--` and `/*` inside brackets start no comment: the `;` after the
        // identifier still splits instead of being swallowed to end of line.
        assert_eq!(
            split_mssql("SELECT [a--b]; SELECT 2"),
            ["SELECT [a--b];", "SELECT 2"]
        );
        assert_eq!(
            split_mssql("SELECT [a/*b]; SELECT 2"),
            ["SELECT [a/*b];", "SELECT 2"]
        );
        // Quotes inside brackets are plain characters.
        assert_eq!(
            split_mssql("SELECT [a'b]; SELECT 2"),
            ["SELECT [a'b];", "SELECT 2"]
        );
    }

    #[test]
    fn doubled_closing_brackets_stay_inside_the_identifier() {
        // `]]` is an escaped `]`: the identifier is `a]b`, and the `;` after
        // the real closing bracket still splits.
        assert_eq!(
            split_mssql("SELECT [a]]b]; SELECT 2"),
            ["SELECT [a]]b];", "SELECT 2"]
        );
        assert_eq!(
            split_mssql("SELECT [a]];b]; SELECT 2"),
            ["SELECT [a]];b];", "SELECT 2"]
        );
    }

    #[test]
    fn go_inside_brackets_does_not_split() {
        assert_eq!(split_mssql("SELECT [a\nGO\nb]"), ["SELECT [a\nGO\nb]"]);
        // A GO line right after a multi-line bracketed identifier still
        // splits: the code newline after the `]` resets line tracking.
        assert_eq!(
            split_mssql("SELECT [a\nb]\nGO\nSELECT 2"),
            ["SELECT [a\nb]", "SELECT 2"]
        );
    }

    #[test]
    fn brackets_inside_strings_and_comments_do_not_open_bracket_mode() {
        // A '[' in a string is literal text — the following `;` still splits.
        assert_eq!(
            split_mssql("SELECT '['; SELECT 2"),
            ["SELECT '[';", "SELECT 2"]
        );
        assert_eq!(
            split_mssql("SELECT \"[\"; SELECT 2"),
            ["SELECT \"[\";", "SELECT 2"]
        );
        // Likewise inside line and block comments.
        assert_eq!(
            split_mssql("SELECT 1 -- [\n; SELECT 2"),
            ["SELECT 1 -- [\n;", "SELECT 2"]
        );
        assert_eq!(
            split_mssql("SELECT 1 /* [ */; SELECT 2"),
            ["SELECT 1 /* [ */;", "SELECT 2"]
        );
    }

    #[test]
    fn unterminated_brackets_swallow_the_rest() {
        // Same graceful degradation as an unterminated string: the remainder
        // is one statement.
        assert_eq!(split_mssql("SELECT [a; b"), ["SELECT [a; b"]);
        assert_eq!(split_mssql("SELECT [a]]; b"), ["SELECT [a]]; b"]);
        assert_eq!(split_mssql("SELECT [a\nGO\nb"), ["SELECT [a\nGO\nb"]);
    }

    #[test]
    fn brackets_are_identifiers_on_sqlite_and_subscripts_on_postgres() {
        // SQLite reads `[…]` as a quoted identifier, so a `;` inside is part
        // of the name. Postgres uses `[` for array subscripts: plain code.
        assert_eq!(
            split_statements("SELECT [a;b] FROM t", Dialect::Sqlite),
            ["SELECT [a;b] FROM t"]
        );
        assert_eq!(
            split_statements("SELECT [a;b] FROM t", Dialect::Postgres),
            ["SELECT [a", "b] FROM t"]
        );
    }

    #[test]
    fn go_lines_do_not_split_on_other_dialects() {
        for dialect in [Dialect::Sqlite, Dialect::Postgres] {
            assert_eq!(
                split_statements("SELECT 1\nGO\nSELECT 2", dialect),
                ["SELECT 1\nGO\nSELECT 2"],
                "{dialect:?}"
            );
        }
    }

    #[test]
    fn reads_are_classified_by_first_keyword() {
        for sql in [
            "SELECT 1",
            "select 1",
            "  WITH x AS (SELECT 1) SELECT * FROM x",
            "VALUES (1)",
            "SHOW server_version",
            "EXPLAIN SELECT 1",
            "PRAGMA table_info(t)",
            "TABLE t",
            "(SELECT 1)",
            "-- comment first\nSELECT 1",
            "/* block */ SELECT 1",
        ] {
            for dialect in [Dialect::Sqlite, Dialect::Postgres, Dialect::SqlServer] {
                assert_eq!(
                    classify_statement(sql, dialect),
                    StatementKind::Read,
                    "{sql:?}"
                );
            }
        }
    }

    #[test]
    fn schema_changing_statements_are_classified_as_ddl() {
        for sql in [
            "CREATE TABLE t (a int)",
            "create index i on t (a)",
            "DROP TABLE t",
            "ALTER TABLE t ADD b int",
            "TRUNCATE t",
            "GRANT ALL ON t TO x",
            "REVOKE ALL ON t FROM x",
            "-- comment first\nDROP TABLE t",
        ] {
            for dialect in [Dialect::Sqlite, Dialect::Postgres, Dialect::SqlServer] {
                assert_eq!(
                    classify_statement(sql, dialect),
                    StatementKind::Ddl,
                    "{sql:?}"
                );
            }
        }
    }

    #[test]
    fn everything_else_is_a_write() {
        for sql in [
            "INSERT INTO t VALUES (1)",
            "UPDATE t SET a = 1",
            "DELETE FROM t",
            "BEGIN",
            "VACUUM",
            "COPY t FROM stdin",
            "", // unclassifiable: err on the safe side
        ] {
            for dialect in [Dialect::Sqlite, Dialect::Postgres, Dialect::SqlServer] {
                assert_eq!(
                    classify_statement(sql, dialect),
                    StatementKind::Write,
                    "{sql:?}"
                );
            }
        }
    }

    #[test]
    fn reads_need_no_capability_beyond_querying() {
        for sql in [
            "SELECT 1",
            "WITH x AS (SELECT 1) SELECT * FROM x",
            "EXPLAIN SELECT 1",
            "VALUES (1)",
            "TABLE t",
        ] {
            assert_eq!(
                statement_needs(sql, Dialect::Postgres),
                StatementNeeds::default(),
                "{sql:?}"
            );
        }
    }

    #[test]
    fn statement_needs_separates_data_changes_from_schema_changes() {
        let needs = |sql| statement_needs(sql, Dialect::Postgres);
        // Plain DML needs mutate only.
        for sql in ["INSERT INTO t VALUES (1)", "UPDATE t SET a = 1", "DELETE t"] {
            assert_eq!(
                needs(sql),
                StatementNeeds {
                    mutate: true,
                    ddl: false
                },
                "{sql:?}"
            );
        }
        // Plain DDL needs ddl only, so a connection that permits row edits
        // but not schema changes can still run its INSERTs.
        for sql in [
            "CREATE TABLE t (a int)",
            "DROP TABLE t",
            "ALTER TABLE t ADD b int",
        ] {
            assert_eq!(
                needs(sql),
                StatementNeeds {
                    mutate: false,
                    ddl: true
                },
                "{sql:?}"
            );
        }
        // TRUNCATE is the exception: DDL by classification, but it empties
        // the table, so it needs the row-changing capability as well.
        assert_eq!(
            needs("TRUNCATE t"),
            StatementNeeds {
                mutate: true,
                ddl: true
            }
        );
    }

    #[test]
    fn select_into_is_caught_inside_a_cte_or_explain() {
        // Regression: the WITH/EXPLAIN scan looked only for DML and DDL
        // keywords, so `SELECT … INTO` — which names neither, but creates a
        // table — slipped through and ran on a read-only connection.
        for dialect in [Dialect::Postgres, Dialect::SqlServer] {
            for sql in [
                "WITH x AS (SELECT 1 AS a) SELECT * INTO t2 FROM x",
                "EXPLAIN ANALYZE SELECT * INTO t2 FROM t",
            ] {
                assert!(needs_confirmation(sql, dialect), "{sql:?}");
                assert_eq!(
                    statement_needs(sql, dialect),
                    StatementNeeds {
                        mutate: true,
                        ddl: true
                    },
                    "{sql:?}"
                );
                assert!(
                    script_refusal(Capabilities::FULL.read_only(), &stmts(&[sql]), dialect)
                        .is_some(),
                    "a read-only connection must refuse {sql:?}"
                );
            }
            // Reads that merely mention neither form still don't prompt.
            for sql in ["EXPLAIN SELECT 1", "WITH x AS (SELECT 1) SELECT * FROM x"] {
                assert!(!needs_confirmation(sql, dialect), "{sql:?}");
            }
        }
    }

    #[test]
    fn bare_transaction_control_needs_nothing_but_its_contents_still_count() {
        for dialect in [Dialect::Sqlite, Dialect::Postgres, Dialect::SqlServer] {
            // `SAVEPOINT` is not T-SQL (its spelling is `SAVE TRAN`), so on
            // SQL Server it is a syntax error and is charged like any word
            // nothing here knows.
            let savepoint = (dialect != Dialect::SqlServer).then_some("SAVEPOINT s");
            for sql in ["BEGIN", "COMMIT", "ROLLBACK", "BEGIN TRANSACTION"]
                .into_iter()
                .chain(savepoint)
            {
                assert_eq!(
                    statement_needs(sql, dialect),
                    StatementNeeds::default(),
                    "{sql:?} on {dialect:?}"
                );
            }
        }
        // A T-SQL block opening with BEGIN is not bare: its DELETE counts.
        assert!(statement_needs("BEGIN DELETE FROM t END", Dialect::SqlServer).mutate);
        // And a read-only connection still refuses the script as a whole.
        assert!(script_refusal(
            Capabilities::FULL.read_only(),
            &stmts(&["BEGIN", "DELETE FROM t", "COMMIT"]),
            Dialect::Postgres
        )
        .is_some());
    }

    #[test]
    fn an_unrecognized_statement_needs_both_capabilities() {
        // Nothing here knows what `EXEC`/`CALL` do, and classifying them as
        // plain writes for dispatch must not imply they only change rows.
        for sql in ["EXEC sp_rename 'a', 'b'", "CALL some_proc()", "VACUUM", ""] {
            assert_eq!(
                statement_needs(sql, Dialect::SqlServer),
                StatementNeeds {
                    mutate: true,
                    ddl: true
                },
                "{sql:?}"
            );
        }
    }

    #[test]
    fn a_write_buried_in_a_read_still_needs_the_capability() {
        // The first keyword says WITH, but the statement deletes rows.
        assert!(
            statement_needs(
                "WITH gone AS (DELETE FROM t RETURNING *) SELECT * FROM gone",
                Dialect::Postgres
            )
            .mutate
        );
        // EXPLAIN ANALYZE really runs the statement on Postgres.
        assert!(statement_needs("EXPLAIN ANALYZE UPDATE t SET a = 1", Dialect::Postgres).mutate);
        // A CTE that creates a table needs the ddl capability.
        assert!(
            statement_needs(
                "WITH x AS (SELECT 1) CREATE TABLE t AS SELECT * FROM x",
                Dialect::Postgres
            )
            .ddl
        );
        // A quoted keyword is not a write.
        assert_eq!(
            statement_needs("SELECT 'please do not DELETE me'", Dialect::Postgres),
            StatementNeeds::default()
        );
    }

    /// Every statement the plan view might be pointed at, including the ones
    /// written to look harmless.
    const GATE_CORPUS: [&str; 22] = [
        "SELECT 1",
        "SELECT * FROM t WHERE a = 1",
        "TABLE t",
        "VALUES (1)",
        "SELECT 'DELETE FROM t'",
        "SELECT 1 -- DELETE FROM t",
        "SELECT /* DROP TABLE t */ 1",
        "UPDATE t SET a = 1",
        "DELETE FROM t",
        "INSERT INTO t VALUES (1)",
        "TRUNCATE t",
        "DROP TABLE t",
        "CREATE TABLE t (a int)",
        "SELECT * INTO backup FROM t",
        "WITH gone AS (DELETE FROM t RETURNING *) SELECT * FROM gone",
        "WITH x AS (SELECT 1) SELECT * INTO t2 FROM x",
        "CALL do_the_thing()",
        "EXEC sp_rename 'a', 'b'",
        "PRAGMA journal_mode = WAL",
        "VACUUM",
        "COPY t FROM stdin",
        "REFRESH MATERIALIZED VIEW mv",
    ];

    /// **The plan view cannot run a write the Run button would have prompted
    /// for**, as a property rather than a list of cases.
    ///
    /// The Explain action's whole rewrite is prefixing `EXPLAIN …`
    /// ([`explain_statement`](super::super::plan::explain_statement)), and it
    /// then hands the result to the same [`script_refusal`] /
    /// [`needs_confirmation`] gates a typed statement goes through. So the one
    /// thing that could make it a bypass is a prefix that *lowers* what a
    /// statement is charged for — which is exactly what this asserts cannot
    /// happen, for every prefix on every dialect.
    ///
    /// It found a real hole: before [`explain_body`], `EXPLAIN CALL p()` was
    /// charged nothing at all, because the keyword scan looks only for write
    /// forms it can name and `CALL` names none. Three characters in front of a
    /// statement turned "needs both capabilities" into "needs none".
    #[test]
    fn only_an_explain_gets_the_plan_cell_cap() {
        use crate::db::plan::{explain_statement, ExplainSupport};
        for support in [
            ExplainSupport::PG_JSON,
            ExplainSupport::PG_TEXT,
            ExplainSupport::SQLITE,
        ] {
            let plan = explain_statement("SELECT * FROM t", support);
            assert_eq!(cell_cap(&plan, support.dialect), PLAN_CELL_CAP, "{plan:?}");
        }
        let all = [Dialect::Sqlite, Dialect::Postgres, Dialect::SqlServer];
        for dialect in all {
            for sql in [
                "EXPLAIN (ANALYZE, FORMAT JSON) SELECT 1",
                "  -- a comment\nexplain select 1",
                "(EXPLAIN SELECT 1)",
            ] {
                assert_eq!(
                    cell_cap(sql, dialect),
                    PLAN_CELL_CAP,
                    "{sql:?} on {dialect:?}"
                );
            }
            // Everything else keeps the display cap, including a statement
            // that only mentions EXPLAIN.
            for sql in [
                "SELECT * FROM t",
                "WITH x AS (SELECT 1) SELECT * FROM x",
                "SELECT 'EXPLAIN'",
                "/* EXPLAIN */ SELECT 1",
                "SELECT explain FROM t",
                "PRAGMA table_info(t)",
                "VALUES (1)",
            ] {
                assert_eq!(
                    cell_cap(sql, dialect),
                    QUERY_CELL_CAP,
                    "{sql:?} on {dialect:?}"
                );
            }
        }
        // Read in the engine's own dialect: Postgres and SQL Server nest
        // block comments, so this EXPLAIN is commented out and the SELECT
        // runs; SQLite does not, so there it is the EXPLAIN that runs.
        // Read as SQLite everywhere, the SELECT got the plan cap (measured:
        // a 200,000-byte cell on Postgres).
        let nested = "/* /* */ EXPLAIN */ SELECT repeat('Z', 200000)";
        assert_eq!(cell_cap(nested, Dialect::Postgres), QUERY_CELL_CAP);
        assert_eq!(cell_cap(nested, Dialect::SqlServer), QUERY_CELL_CAP);
        assert_eq!(cell_cap(nested, Dialect::Sqlite), PLAN_CELL_CAP);
    }

    #[test]
    fn explaining_never_loosens_the_gate() {
        use super::super::plan::{explain_statement, ExplainSupport};
        for dialect in [Dialect::Sqlite, Dialect::Postgres, Dialect::SqlServer] {
            for support in [
                ExplainSupport::PG_JSON,
                ExplainSupport::PG_TEXT,
                ExplainSupport::SQLITE,
            ] {
                for sql in GATE_CORPUS {
                    let explained = explain_statement(sql, support);
                    let plain_needs = statement_needs(sql, dialect);
                    let explained_needs = statement_needs(&explained, dialect);
                    assert!(
                        !plain_needs.mutate || explained_needs.mutate,
                        "{explained:?} on {dialect:?} lost the mutate gate {sql:?} had"
                    );
                    assert!(
                        !plain_needs.ddl || explained_needs.ddl,
                        "{explained:?} on {dialect:?} lost the ddl gate {sql:?} had"
                    );
                    assert!(
                        !needs_confirmation(sql, dialect)
                            || needs_confirmation(&explained, dialect),
                        "{explained:?} on {dialect:?} stopped prompting for {sql:?}"
                    );
                    // The same statement through the capability gate the UI
                    // actually calls: whatever a read-only connection refuses
                    // to run, it refuses to explain.
                    let read_only = Capabilities::FULL.read_only();
                    assert!(
                        script_refusal(read_only, &stmts(&[sql]), dialect).is_none()
                            || script_refusal(read_only, &stmts(&[&explained]), dialect).is_some(),
                        "a read-only connection refuses {sql:?} but would explain it as \
                         {explained:?} on {dialect:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn an_explain_inherits_what_it_explains() {
        // The specific shapes behind the property above, spelled out so a
        // regression names itself.
        for (sql, dialect) in [
            // The hole `explain_body` closes: an effect nothing can name.
            ("EXPLAIN CALL do_the_thing()", Dialect::Postgres),
            (
                "EXPLAIN (FORMAT JSON) CALL do_the_thing()",
                Dialect::Postgres,
            ),
            ("EXPLAIN EXEC sp_rename 'a', 'b'", Dialect::SqlServer),
            // A value-setting PRAGMA behind SQLite's EXPLAIN header.
            (
                "EXPLAIN QUERY PLAN PRAGMA journal_mode = WAL",
                Dialect::Sqlite,
            ),
            // Named write forms, with the header spelled every way.
            ("EXPLAIN ANALYZE UPDATE t SET a = 1", Dialect::Postgres),
            (
                "EXPLAIN (ANALYZE, FORMAT JSON) DELETE FROM t",
                Dialect::Postgres,
            ),
            ("explain analyze verbose delete from t", Dialect::Postgres),
            // Comments between the header words hide nothing: the scan runs
            // on text with comments already blanked out.
            (
                "EXPLAIN /* just looking */ ANALYZE /* honest */ UPDATE t SET a = 1",
                Dialect::Postgres,
            ),
            // A data-modifying CTE under an EXPLAIN ANALYZE.
            (
                "EXPLAIN ANALYZE WITH gone AS (DELETE FROM t RETURNING *) SELECT * FROM gone",
                Dialect::Postgres,
            ),
            // An unterminated option list leaves no readable body, which is
            // charged as an unknown effect rather than waved through.
            ("EXPLAIN (FORMAT JSON SELECT 1", Dialect::Postgres),
        ] {
            assert!(needs_confirmation(sql, dialect), "{sql:?}");
            assert!(
                script_refusal(Capabilities::FULL.read_only(), &stmts(&[sql]), dialect).is_some(),
                "a read-only connection must refuse {sql:?}"
            );
        }
    }

    /// Ways a server lets you spell the part between `EXPLAIN` and the
    /// statement it explains — PostgreSQL's option list and its legacy bare
    /// options, SQLite's `EXPLAIN QUERY PLAN`, in the casings people write.
    ///
    /// **This is where a newly-learned spelling goes.** The list is grounded
    /// in a real server by `postgres_accepts_every_explain_header_hubro_tolerates`
    /// (tests/db_postgres.rs), so it cannot drift into syntax that only
    /// hubro believes in.
    const EXPLAIN_HEADERS: [&str; 14] = [
        "EXPLAIN",
        "explain",
        "EXPLAIN ANALYZE",
        // PostgreSQL's grammar takes the British spelling as a synonym. Its
        // absence from `EXPLAIN_OPTION_WORDS` made `EXPLAIN ANALYSE SELECT 1`
        // a refusal on a read-only connection — the regression this property
        // exists to have caught.
        "EXPLAIN ANALYSE",
        "EXPLAIN VERBOSE",
        "EXPLAIN ANALYZE VERBOSE",
        "explain analyse verbose",
        "EXPLAIN (FORMAT JSON)",
        "EXPLAIN (ANALYZE, FORMAT JSON)",
        "EXPLAIN (ANALYZE, BUFFERS, VERBOSE, FORMAT JSON)",
        "EXPLAIN (COSTS OFF)",
        "EXPLAIN /* just looking */ ANALYZE",
        "EXPLAIN QUERY PLAN",
        "explain query plan",
    ];

    /// **Explaining a read charges it nothing** — the counterpart to
    /// [`explaining_never_loosens_the_gate`], over the same corpus.
    ///
    /// Both directions matter and only one of them was a property before.
    /// Loosening the gate lets a write through; tightening it refuses a
    /// `SELECT` on a read-only connection with a reason that doesn't describe
    /// it, and trains the user to click through the banner that does. The
    /// second failure is quieter, which is exactly why it needs the property
    /// rather than a list of remembered cases — a fixed list is how
    /// `ANALYSE` shipped.
    ///
    /// The reads are *derived* from [`GATE_CORPUS`] rather than written out
    /// again, so a read added there is covered here without anyone
    /// remembering to.
    #[test]
    fn explaining_a_read_never_charges_it() {
        for dialect in [Dialect::Sqlite, Dialect::Postgres, Dialect::SqlServer] {
            let reads: Vec<&str> = GATE_CORPUS
                .into_iter()
                .filter(|sql| {
                    statement_needs(sql, dialect) == StatementNeeds::default()
                        && !needs_confirmation(sql, dialect)
                })
                .collect();
            // A property over an empty set proves nothing; a corpus that
            // stopped containing reads would leave this passing and checking
            // nothing at all.
            assert!(reads.len() >= 5, "{dialect:?}: {reads:?}");
            for header in EXPLAIN_HEADERS {
                for read in &reads {
                    let explained = format!("{header} {read}");
                    assert!(
                        !needs_confirmation(&explained, dialect),
                        "{explained:?} on {dialect:?} prompts to confirm a read"
                    );
                    assert_eq!(
                        statement_needs(&explained, dialect),
                        StatementNeeds::default(),
                        "{explained:?} on {dialect:?} charges a read a capability"
                    );
                    // The gate the SQL pane actually calls: a read-only
                    // connection runs reads, explained or not.
                    assert_eq!(
                        script_refusal(
                            Capabilities::FULL.read_only(),
                            &stmts(&[&explained]),
                            dialect
                        ),
                        None,
                        "a read-only connection refuses to explain the read {read:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn explaining_a_read_still_prompts_for_nothing() {
        // The other half of the guard: over-prompting on the statements the
        // Explain button actually generates would train the user to click
        // through the banner that matters.
        for (sql, dialect) in [
            ("EXPLAIN SELECT 1", Dialect::Postgres),
            (
                "EXPLAIN (FORMAT JSON) SELECT * FROM t WHERE a = 1",
                Dialect::Postgres,
            ),
            (
                "EXPLAIN (ANALYZE, FORMAT JSON, BUFFERS) SELECT 1",
                Dialect::Postgres,
            ),
            ("EXPLAIN ANALYZE SELECT 1", Dialect::Postgres),
            ("EXPLAIN VERBOSE SELECT count(*) FROM t", Dialect::Postgres),
            ("EXPLAIN (FORMAT JSON) TABLE t", Dialect::Postgres),
            ("EXPLAIN (FORMAT JSON) VALUES (1)", Dialect::Postgres),
            (
                "EXPLAIN (FORMAT JSON) WITH x AS (SELECT 1) SELECT * FROM x",
                Dialect::Postgres,
            ),
            ("EXPLAIN QUERY PLAN SELECT * FROM t", Dialect::Sqlite),
            // Function calls in a read are not the PRAGMA `(` rule.
            (
                "EXPLAIN QUERY PLAN SELECT lower(name) FROM t",
                Dialect::Sqlite,
            ),
            // A write named only inside a string or a comment is not a write.
            (
                "EXPLAIN (FORMAT JSON) SELECT 'DELETE FROM t'",
                Dialect::Postgres,
            ),
            (
                "EXPLAIN (FORMAT JSON) SELECT 1 -- DROP TABLE t",
                Dialect::Postgres,
            ),
        ] {
            assert!(!needs_confirmation(sql, dialect), "{sql:?}");
            assert_eq!(
                statement_needs(sql, dialect),
                StatementNeeds::default(),
                "{sql:?}"
            );
        }
    }

    #[test]
    fn a_semicolon_pair_is_explained_statement_by_statement() {
        use super::super::plan::{explain_statement, ExplainSupport};
        // The write in the second half of the buffer keeps its gate: Explain
        // splits first and prefixes each statement, so nothing is hidden
        // behind the first one's EXPLAIN.
        let script = "SELECT 1; DELETE FROM t";
        let explained: Vec<String> = split_statements(script, Dialect::Postgres)
            .iter()
            .map(|statement| explain_statement(statement, ExplainSupport::PG_JSON))
            .collect();
        assert_eq!(
            explained,
            [
                "EXPLAIN (FORMAT JSON) SELECT 1",
                "EXPLAIN (FORMAT JSON) DELETE FROM t"
            ]
        );
        assert_eq!(
            script_refusal(
                Capabilities::FULL.read_only(),
                &explained,
                Dialect::Postgres
            ),
            Some((1, caps::NO_MUTATE))
        );
        // And on a connection that can write, it is the confirmation banner
        // that stands between the plan view and the DELETE.
        assert!(explained
            .iter()
            .any(|s| needs_confirmation(s, Dialect::Postgres)));
    }

    #[test]
    fn an_unattributable_change_needs_both_capabilities() {
        // SELECT … INTO creates a table and names neither keyword set; a
        // PRAGMA changes state without naming either. Both are charged to
        // both capabilities rather than guessed at. The call form sets just
        // as `=` does — each of these was measured writing the file, or
        // refused on one opened read-only — so it is charged the same.
        for sql in [
            "SELECT * INTO backup FROM t",
            "PRAGMA journal_mode = WAL",
            "PRAGMA user_version(5)",
            "PRAGMA main.user_version(5)",
            "PRAGMA application_id(7)",
            "PRAGMA journal_mode(WAL)",
            "PRAGMA schema_version(9)",
            "PRAGMA optimize(0x10002)",
            "PRAGMA wal_checkpoint(PASSIVE)",
            "PRAGMA incremental_vacuum(1)",
            // A reader's name in a shape that is not its call form.
            "PRAGMA table_info(t) = 1",
            "PRAGMA table_info(user_version(5))",
            "PRAGMA a.b.table_info(t)",
            "PRAGMA user_version = 5 + x.table_info(t)",
            "PRAGMA table_info(a = 1)",
            "PRAGMA x-1.table_info(t)",
        ] {
            assert_eq!(
                statement_needs(sql, Dialect::Sqlite),
                StatementNeeds {
                    mutate: true,
                    ddl: true
                },
                "{sql:?}"
            );
        }
        // The other side: reads that only *look* unattributable are not
        // charged, so a connection marked read-only can still run them.
        // Charging them both refused `PRAGMA table_info(t)` there outright —
        // the "fail-safe" over-prompt became a refusal with no prompt.
        for (sql, dialect) in [
            ("PRAGMA table_info(t)", Dialect::Sqlite),
            ("pragma TABLE_INFO ( 't' );", Dialect::Sqlite),
            ("PRAGMA main.table_xinfo(t)", Dialect::Sqlite),
            ("PRAGMA \"main\".index_list(t)", Dialect::Sqlite),
            ("PRAGMA index_info(ix)", Dialect::Sqlite),
            ("PRAGMA index_xinfo(ix)", Dialect::Sqlite),
            ("PRAGMA foreign_key_list(t)", Dialect::Sqlite),
            ("PRAGMA foreign_key_check(t)", Dialect::Sqlite),
            ("PRAGMA integrity_check(5)", Dialect::Sqlite),
            ("PRAGMA integrity_check(-1)", Dialect::Sqlite),
            ("PRAGMA quick_check(t)", Dialect::Sqlite),
            ("PRAGMA table_list(t)", Dialect::Sqlite),
            // `replace(` is the string function, not the statement.
            (
                "WITH x AS (SELECT replace(name, '-', ' ') AS n FROM t) SELECT * FROM x",
                Dialect::Postgres,
            ),
            (
                "WITH x AS (SELECT REPLACE /* c */ (name, 'a', 'b') AS n FROM t) SELECT n FROM x",
                Dialect::Sqlite,
            ),
            (
                "EXPLAIN SELECT replace(name, 'a', 'b') FROM t",
                Dialect::Postgres,
            ),
        ] {
            assert_eq!(
                statement_needs(sql, dialect),
                StatementNeeds::default(),
                "{sql:?} on {dialect:?}"
            );
            assert!(!needs_confirmation(sql, dialect), "{sql:?} on {dialect:?}");
        }
        // SQL Server never reaches the pragma reading — it has no pragmas,
        // and every read there is scanned whole, since T-SQL needs no `;`
        // between statements. Pinned because the arm order is what keeps it
        // so: a `PRAGMA` arm checked first would read this as a bare pragma.
        assert_eq!(
            statement_needs("PRAGMA DELETE FROM t", Dialect::SqlServer),
            StatementNeeds {
                mutate: true,
                ddl: false
            }
        );
        assert!(statement_needs("PRAGMA table_info(t) DELETE FROM t", Dialect::SqlServer).mutate);
    }

    #[test]
    fn embedded_writes_in_fetch_statements_need_confirmation() {
        for sql in [
            // Data-modifying CTEs execute their writes.
            "WITH moved AS (DELETE FROM t RETURNING *) SELECT * FROM moved",
            "with x as (insert into t values (1) returning id) select * from x",
            "WITH x AS (UPDATE t SET a = 1 RETURNING a) SELECT * FROM x",
            // Postgres actually executes the statement under EXPLAIN ANALYZE.
            "EXPLAIN ANALYZE UPDATE t SET a = 1",
            "EXPLAIN (ANALYZE, BUFFERS) DELETE FROM t",
            // SELECT INTO creates a table.
            "SELECT * INTO new_table FROM t",
            "select a, b into backup from t where a > 1",
            // Value-setting pragmas mutate the database file.
            "PRAGMA journal_mode = WAL",
            "PRAGMA busy_timeout(5000)",
        ] {
            for dialect in [Dialect::Sqlite, Dialect::Postgres] {
                assert!(
                    needs_confirmation(sql, dialect),
                    "{sql:?} must need confirmation on {dialect:?}"
                );
            }
        }
    }

    #[test]
    fn plain_reads_do_not_need_confirmation() {
        for sql in [
            "SELECT * FROM t",
            "SELECT a AS into_marker FROM t", // 'into' only as part of a word
            "WITH x AS (SELECT 1) SELECT * FROM x",
            "EXPLAIN SELECT * FROM t",
            "EXPLAIN ANALYZE SELECT * FROM t",
            "PRAGMA journal_mode", // bare query form reads the setting
            "VALUES (1)",
            "SHOW server_version",
            "TABLE t",
            // Write keywords inside literals and comments must not trigger.
            "SELECT 'please DELETE me later; INTO the bin' FROM t",
            "WITH x AS (SELECT 'drop table users') SELECT * FROM x",
            "EXPLAIN SELECT 1 -- update later\n",
            "WITH x AS (SELECT 1) SELECT * FROM x /* create index? */",
            "SELECT \"into\" FROM t", // quoted identifier
        ] {
            for dialect in [Dialect::Sqlite, Dialect::Postgres] {
                assert!(
                    !needs_confirmation(sql, dialect),
                    "{sql:?} must not prompt on {dialect:?}"
                );
            }
        }
    }

    #[test]
    fn classified_writes_always_need_confirmation() {
        for sql in ["INSERT INTO t VALUES (1)", "DROP TABLE t", "BEGIN", ""] {
            for dialect in [Dialect::Sqlite, Dialect::Postgres, Dialect::SqlServer] {
                assert!(
                    needs_confirmation(sql, dialect),
                    "{sql:?} must need confirmation on {dialect:?}"
                );
            }
        }
    }

    #[test]
    fn bracketed_identifiers_are_lexed_for_confirmation_on_sqlserver() {
        // A quote inside a bracketed identifier must not invert string
        // tracking: the INTO after it is real, and skipping the prompt here
        // was the fail-unsafe bug this covers.
        assert!(needs_confirmation(
            "SELECT [o'brien] * INTO t2 FROM t",
            Dialect::SqlServer
        ));
        // Keywords inside brackets are identifier text, not tokens — no
        // over-prompting.
        for sql in [
            "SELECT [into] FROM t",
            "WITH x AS (SELECT 1) SELECT [delete] FROM x",
            // `]]` is an escaped `]`: the identifier is `a]into`, still text.
            "SELECT [a]]into] FROM t",
        ] {
            assert!(
                !needs_confirmation(sql, Dialect::SqlServer),
                "{sql:?} must not prompt"
            );
        }
        // A real INTO after a bracketed identifier still prompts.
        assert!(needs_confirmation(
            "SELECT [a], b INTO t2 FROM t",
            Dialect::SqlServer
        ));
    }

    #[test]
    fn brackets_are_lexed_for_confirmation_on_sqlite_only() {
        // `[into]` is a name on SQLite, as on SQL Server; on Postgres it is
        // code, so the word `into` over-prompts (the fail-safe side).
        assert!(!needs_confirmation("SELECT [into] FROM t", Dialect::Sqlite));
        assert!(needs_confirmation(
            "SELECT [into] FROM t",
            Dialect::Postgres
        ));
    }

    #[test]
    fn strip_strings_and_comments_blanks_only_literals_and_comments() {
        let stripped =
            strip_strings_and_comments("SELECT 'a;b', \"q\" -- c\nFROM t /* x */", Dialect::Sqlite);
        assert!(stripped.contains("SELECT"));
        assert!(stripped.contains("FROM t"));
        for gone in ["a;b", "q", "c", "x", "'", "\"", "--", "/*"] {
            assert!(!stripped.contains(gone), "{gone:?} should be blanked");
        }
        let stripped =
            strip_strings_and_comments("SELECT $$drop$$, $t$delete$t$, $1", Dialect::Postgres);
        assert!(!stripped.contains("drop"));
        assert!(!stripped.contains("delete"));
        // A parameter placeholder survives.
        assert!(stripped.contains("$1"));
        // Multibyte text stays valid UTF-8, and the byte length is kept —
        // except on SQL Server, where a number glued to a word gets a space.
        let input = "SELECT übercol, 'смузи' FROM t";
        let stripped = strip_strings_and_comments(input, Dialect::Sqlite);
        assert_eq!(stripped.len(), input.len());
        assert!(stripped.contains("übercol"));
        assert!(!stripped.contains("смузи"));
        let stripped = strip_strings_and_comments("SELECT 1DELETE", Dialect::SqlServer);
        assert_eq!(stripped, "SELECT 1 DELETE");
    }

    #[test]
    fn strip_blanks_bracketed_identifiers_on_sqlserver_only() {
        let stripped = strip_strings_and_comments("SELECT [o'brien], x FROM t", Dialect::SqlServer);
        assert!(!stripped.contains("brien"));
        assert!(!stripped.contains('\''));
        // Code after the bracket survives.
        assert!(stripped.contains("x FROM t"));
        // Unterminated bracket swallows the rest, like an unterminated string.
        let stripped = strip_strings_and_comments("SELECT [a INTO b", Dialect::SqlServer);
        assert!(!stripped.contains("INTO"));
        // On other dialects brackets are plain text and stay in place.
        let stripped = strip_strings_and_comments("SELECT [into] FROM t", Dialect::Postgres);
        assert!(stripped.contains("[into]"));
    }

    /// Each region boundary is the engine's own (measured; see `region_at`).
    #[test]
    fn lexer_boundaries_follow_each_engine() {
        use Dialect::{Postgres as Pg, SqlServer as Ms, Sqlite as Lite};
        use RegionKind::*;
        // Backticks quote names on SQLite only.
        assert_eq!(regions("`a'b` x", Lite), [("`a'b`", String), (" x", Code)]);
        assert_eq!(regions("`a` x", Pg), [("`a` x", Code)]);
        // SQLite brackets have no `]]` escape.
        assert_eq!(regions("[a]]b", Lite), [("[a]", Bracket), ("]b", Code)]);
        // Block comments nest on Postgres and SQL Server, not SQLite.
        for dialect in [Pg, Ms] {
            assert_eq!(
                regions("/* /* */ x */ y", dialect),
                [("/* /* */ x */", Comment), (" y", Code)],
                "{dialect:?}"
            );
        }
        assert_eq!(
            regions("/* /* */ x */ y", Lite),
            [("/* /* */", Comment), (" x */ y", Code)]
        );
        // A bare CR ends a line comment on Postgres and SQL Server only.
        for dialect in [Pg, Ms] {
            assert_eq!(
                regions("--c\rx", dialect),
                [("--c", Comment), ("\rx", Code)],
                "{dialect:?}"
            );
        }
        assert_eq!(regions("--c\rx", Lite), [("--c\rx", Comment)]);
        // Unicode line breaks end no comment, even on SQL Server (measured
        // with U+0085, U+2028 and U+2029: `--c<char>, 2 AS b` is one column).
        for brk in ["\u{85}", "\u{2028}", "\u{2029}"] {
            let sql = format!("--c{brk}x");
            assert_eq!(regions(&sql, Ms), [(sql.as_str(), Comment)], "{brk:?}");
        }
        // Dollar quotes: Postgres only, never inside a name, any tag bytes.
        assert_eq!(
            regions("$q$;$q$ x", Pg),
            [("$q$;$q$", DollarQuote), (" x", Code)]
        );
        assert_eq!(regions("x$q$ y", Pg), [("x$q$ y", Code)]);
        assert_eq!(regions("$é$;$é$", Pg), [("$é$;$é$", DollarQuote)]);
        for dialect in [Lite, Ms] {
            assert_eq!(
                regions("$q$;$q$", dialect),
                [("$q$;$q$", Code)],
                "{dialect:?}"
            );
        }
        // Postgres escape strings honour backslashes and continue across a
        // newline; a name ending in E does not start one.
        assert_eq!(
            regions("E'\\'' x", Pg),
            [("E", Code), ("'\\''", String), (" x", Code)]
        );
        assert_eq!(
            regions("e'a'\n -- c\n '\\'' x", Pg),
            [("e", Code), ("'a'\n -- c\n '\\''", String), (" x", Code)]
        );
        assert_eq!(
            regions("nameE'\\' x", Pg),
            [("nameE", Code), ("'\\'", String), (" x", Code)]
        );
        assert_eq!(
            regions("E'\\'' x", Lite),
            [("E", Code), ("'\\'", String), ("' x", String)]
        );
    }

    /// The lexed regions of `sql` as text slices, for asserting on directly.
    fn regions(sql: &str, dialect: Dialect) -> Vec<(&str, RegionKind)> {
        lex_regions(sql, dialect)
            .map(|(range, kind)| (&sql[range], kind))
            .collect()
    }

    #[test]
    fn lexer_covers_every_byte_in_order() {
        use RegionKind::*;
        assert_eq!(
            regions("SELECT 'a;b' -- c\n/* d */ $t$e$t$ $1", Dialect::Postgres),
            [
                ("SELECT ", Code),
                ("'a;b'", String),
                (" ", Code),
                ("-- c", Comment), // the newline is code, for GO line tracking
                ("\n", Code),
                ("/* d */", Comment),
                (" ", Code),
                ("$t$e$t$", DollarQuote),
                (" $1", Code), // a parameter placeholder is not a delimiter
            ]
        );
    }

    #[test]
    fn lexer_brackets_are_sqlserver_only() {
        use RegionKind::*;
        assert_eq!(
            regions("SELECT [a]]b] x", Dialect::SqlServer),
            [("SELECT ", Code), ("[a]]b]", Bracket), (" x", Code)]
        );
        assert_eq!(
            regions("SELECT [a]]b] x", Dialect::Postgres),
            [("SELECT [a]]b] x", Code)]
        );
    }

    #[test]
    fn lexer_doubled_quotes_lex_as_adjacent_strings() {
        use RegionKind::*;
        assert_eq!(
            regions("'it''s'", Dialect::Sqlite),
            [("'it'", String), ("'s'", String)]
        );
    }

    #[test]
    fn lexer_unterminated_regions_swallow_the_rest() {
        use RegionKind::*;
        for (sql, kind) in [
            ("SELECT 'a; b", String),
            ("SELECT /* a /* b */ c", Comment), // unbalanced nesting
            ("SELECT $$a; b", DollarQuote),
        ] {
            assert_eq!(
                regions(sql, Dialect::Postgres),
                [("SELECT ", Code), (&sql[7..], kind)],
                "{sql:?}"
            );
        }
        assert_eq!(
            regions("SELECT [a; b", Dialect::SqlServer),
            [("SELECT ", Code), ("[a; b", Bracket)]
        );
    }

    #[test]
    fn preview_collapses_whitespace_and_truncates_at_60_chars() {
        assert_eq!(
            statement_preview("  SELECT *\n  FROM   t\tWHERE a = 1  "),
            "SELECT * FROM t WHERE a = 1"
        );
        let long = format!("SELECT '{}'", "x".repeat(100));
        let preview = statement_preview(&long);
        assert_eq!(preview.chars().count(), 61);
        assert!(preview.ends_with('…'));
        assert!(preview.starts_with("SELECT 'xxx"));
    }

    fn stmts(sqls: &[&str]) -> Vec<String> {
        sqls.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn a_rollback_claims_less_when_the_engine_lets_schema_changes_escape() {
        let escapes = Capabilities {
            transactional_ddl: false,
            ..Capabilities::FULL
        };
        let dml = stmts(&["INSERT INTO t VALUES (1)", "DELETE FROM t WHERE a = 2"]);
        let with_ddl = stmts(&["INSERT INTO t VALUES (1)", "CREATE TABLE u (id int)"]);

        // A full-featured engine covers everything, schema changes included.
        for script in [&dml, &with_ddl] {
            assert_eq!(
                rollback_cover(Capabilities::FULL, Dialect::Postgres, script),
                Rollback::Full
            );
        }

        // The case this exists for: CockroachDB and YugabyteDB running a
        // script that changes the schema.
        assert_eq!(
            rollback_cover(escapes, Dialect::Postgres, &with_ddl),
            Rollback::ExceptSchemaChanges
        );

        // ...but *only* when there is a schema change to escape. Warning about
        // one that isn't there would be the same mistake pointing the other
        // way: a pure-DML script does roll back completely on those engines.
        assert_eq!(
            rollback_cover(escapes, Dialect::Postgres, &dml),
            Rollback::Full
        );

        // A schema change the first keyword doesn't advertise still counts —
        // `statement_needs` sees what `classify_statement` would call a read.
        let hidden = stmts(&["SELECT 1", "SELECT * INTO backup FROM t"]);
        assert_eq!(
            rollback_cover(escapes, Dialect::Postgres, &hidden),
            Rollback::ExceptSchemaChanges
        );
    }

    #[test]
    fn a_schema_change_the_script_never_reached_did_not_escape() {
        let escapes = Capabilities {
            transactional_ddl: false,
            ..Capabilities::FULL
        };
        // `run_script_atomic` passes the statements up to and including the
        // failing one, which is what makes this distinction reachable: the
        // caller cannot ask about DDL the server never saw.
        let script = stmts(&[
            "INSERT INTO t VALUES (1)",
            "SELECT * FROM missing_relation",
            "CREATE TABLE never (id int)",
        ]);

        // Failing at statement 1, the CREATE TABLE never ran — so the rollback
        // really did cover everything, and reporting a surviving table would
        // invent one.
        assert_eq!(
            rollback_cover(escapes, Dialect::Postgres, &script[..=1]),
            Rollback::Full
        );

        // The failing statement is itself included, because a DDL that fails
        // has still done its damage on CockroachDB: the autocommit fires
        // before the statement runs, so writes staged ahead of it survive.
        let failing_ddl = stmts(&["INSERT INTO t VALUES (1)", "CREATE TABLE dup (id int)"]);
        assert_eq!(
            rollback_cover(escapes, Dialect::Postgres, &failing_ddl[..=1]),
            Rollback::ExceptSchemaChanges
        );
    }

    #[test]
    fn an_atomic_rollback_claims_only_what_it_found_open() {
        let ran = stmts(&[
            "INSERT INTO t VALUES (1)\nCOMMIT",
            "INSERT INTO t VALUES (2)",
        ]);
        let at = |found| atomic_rollback(Capabilities::FULL, Dialect::SqlServer, &ran, found);
        assert_eq!(at(RolledBack::Own), Rollback::Full);
        // The script committed hubro's transaction: "no changes were applied"
        // would hide what it committed.
        assert_eq!(at(RolledBack::Nothing), Rollback::AlreadyEnded);
        assert_eq!(at(RolledBack::Unknown), Rollback::ConnectionClosed);
        assert_eq!(
            at(RolledBack::Another),
            Rollback::OpenTransaction(OpenTransaction {
                schema_changes_kept: false
            })
        );
        let escapes = Capabilities {
            transactional_ddl: false,
            ..Capabilities::FULL
        };
        let ddl = stmts(&["CREATE TABLE t (a int)\nCOMMIT BEGIN TRAN", "SELECT 1/0"]);
        assert_eq!(
            atomic_rollback(escapes, Dialect::SqlServer, &ddl, RolledBack::Another),
            Rollback::OpenTransaction(OpenTransaction {
                schema_changes_kept: true
            })
        );
    }

    #[test]
    fn a_statement_that_commits_the_open_transaction_is_never_wrapped() {
        for sql in ["SHOW COMMIT TIMESTAMP", "show  commit\ttimestamp;"] {
            assert!(
                !wraps(
                    Dialect::Postgres,
                    &stmts(&["INSERT INTO t VALUES (1)", sql])
                ),
                "{sql:?}"
            );
        }
        // Only that statement: other SHOWs are reads like any other.
        assert!(wraps(
            Dialect::Postgres,
            &stmts(&[
                "INSERT INTO t VALUES (1)",
                "SHOW TRANSACTION ISOLATION LEVEL"
            ])
        ));
    }

    #[test]
    fn multi_statement_scripts_wrap_atomically_by_default() {
        for dialect in [Dialect::Sqlite, Dialect::Postgres, Dialect::SqlServer] {
            assert!(wraps(
                dialect,
                &stmts(&["INSERT INTO t VALUES (1)", "UPDATE t SET a = 2"])
            ));
            // Mixed read/write still wraps.
            assert!(wraps(
                dialect,
                &stmts(&["DELETE FROM t WHERE a = 1", "SELECT count(*) FROM t"])
            ));
        }
    }

    #[test]
    fn a_non_transactional_connection_never_wraps() {
        // Without the capability there is no transaction to wrap in, so the
        // script falls back to sequential autocommit instead of failing.
        let caps = Capabilities {
            transactions: false,
            ..Capabilities::FULL
        };
        let script = stmts(&["INSERT INTO t VALUES (1)", "UPDATE t SET a = 2"]);
        for dialect in [Dialect::Sqlite, Dialect::Postgres, Dialect::SqlServer] {
            assert!(wraps(dialect, &script));
            assert!(!wrap_atomically(caps, dialect, &script));
        }
    }

    #[test]
    fn a_read_only_connection_refuses_writes_before_running_anything() {
        let caps = Capabilities::FULL.read_only();
        let script = stmts(&["SELECT 1", "DELETE FROM t", "SELECT 2"]);
        let refusal = refuse_script(caps, &script, Dialect::Postgres).expect("must refuse");
        // The refusal names the offending statement, and nothing ran.
        assert_eq!(refusal.statement_index, 1);
        assert_eq!(refusal.preview, "DELETE FROM t");
        assert_eq!(refusal.rollback, Rollback::None);
        assert_eq!(refusal.error, DbError::Unsupported(caps::NO_MUTATE.into()));
    }

    #[test]
    fn read_query_without_mutate_still_permits_select() {
        let caps = Capabilities::FULL.read_only();
        let script = stmts(&["SELECT 1", "WITH x AS (SELECT 1) SELECT * FROM x", "SHOW x"]);
        assert_eq!(refuse_script(caps, &script, Dialect::Postgres), None);
    }

    #[test]
    fn refusal_distinguishes_schema_changes_from_row_changes() {
        let no_ddl = Capabilities {
            ddl: false,
            ..Capabilities::FULL
        };
        // Row edits are still allowed…
        assert_eq!(
            refuse_script(no_ddl, &stmts(&["DELETE FROM t"]), Dialect::Postgres),
            None
        );
        // …but schema changes are refused, with the schema-specific reason.
        let refusal =
            refuse_script(no_ddl, &stmts(&["DROP TABLE t"]), Dialect::Postgres).expect("refuses");
        assert_eq!(refusal.error, DbError::Unsupported(caps::NO_DDL.into()));
    }

    #[test]
    fn a_connection_that_cannot_query_refuses_everything() {
        let caps = Capabilities {
            read_query: false,
            ..Capabilities::FULL
        };
        let refusal =
            refuse_script(caps, &stmts(&["SELECT 1"]), Dialect::Postgres).expect("refuses");
        assert_eq!(refusal.error, DbError::Unsupported(caps::NO_QUERY.into()));
    }

    #[test]
    fn a_fully_capable_connection_refuses_nothing() {
        let script = stmts(&["SELECT 1", "DELETE FROM t", "CREATE TABLE u (a int)"]);
        for dialect in [Dialect::Sqlite, Dialect::Postgres, Dialect::SqlServer] {
            assert_eq!(refuse_script(Capabilities::FULL, &script, dialect), None);
        }
    }

    #[test]
    fn single_statement_scripts_do_not_wrap() {
        // One statement is already atomic under autocommit — nothing to wrap.
        for dialect in [Dialect::Sqlite, Dialect::Postgres, Dialect::SqlServer] {
            assert!(!wraps(dialect, &stmts(&["DELETE FROM t"])));
        }
    }

    #[test]
    fn scripts_managing_their_own_transaction_do_not_wrap() {
        for dialect in [Dialect::Sqlite, Dialect::Postgres, Dialect::SqlServer] {
            for script in [
                vec!["BEGIN", "INSERT INTO t VALUES (1)", "COMMIT"],
                vec!["begin transaction", "UPDATE t SET a = 1", "commit"],
                vec!["START TRANSACTION", "DELETE FROM t", "ROLLBACK"],
                vec!["SAVEPOINT s", "INSERT INTO t VALUES (1)"],
            ] {
                assert!(
                    !wraps(dialect, &stmts(&script)),
                    "{script:?} manages its own transaction"
                );
            }
        }
    }

    #[test]
    fn non_transactional_statements_prevent_wrapping() {
        // VACUUM can't run inside a transaction on SQLite or Postgres (and
        // doesn't exist on SQL Server, where declining to wrap is harmless).
        for dialect in [Dialect::Sqlite, Dialect::Postgres, Dialect::SqlServer] {
            assert!(!wraps(dialect, &stmts(&["DELETE FROM t", "VACUUM"])));
        }
        // Postgres statements that error inside a transaction block.
        for script in [
            vec![
                "CREATE TABLE t (a int)",
                "CREATE INDEX CONCURRENTLY i ON t (a)",
            ],
            vec!["SELECT 1", "DROP INDEX CONCURRENTLY i"],
            vec!["SELECT 1", "CREATE DATABASE other"],
            vec!["SELECT 1", "DROP DATABASE other"],
            vec!["SELECT 1", "ALTER SYSTEM SET work_mem = '64MB'"],
        ] {
            assert!(
                !wraps(Dialect::Postgres, &stmts(&script)),
                "{script:?} can't run in a transaction on Postgres"
            );
        }
        // `CONCURRENTLY` inside a string/comment must not trip the check.
        assert!(wraps(
            Dialect::Postgres,
            &stmts(&[
                "INSERT INTO t VALUES ('run concurrently later')",
                "UPDATE t SET a = 1",
            ])
        ));
    }

    #[test]
    fn sqlserver_non_transactional_statements_prevent_wrapping() {
        // T-SQL server-level operations that refuse to run inside a user
        // transaction.
        for script in [
            vec!["SELECT 1", "CREATE DATABASE other"],
            vec!["SELECT 1", "ALTER DATABASE other SET RECOVERY SIMPLE"],
            vec!["SELECT 1", "DROP DATABASE other"],
            vec!["SELECT 1", "BACKUP DATABASE db TO DISK = 'x.bak'"],
            vec!["SELECT 1", "RESTORE DATABASE db FROM DISK = 'x.bak'"],
            vec!["SELECT 1", "backup log db to disk = 'x.trn'"],
            vec![
                "CREATE TABLE t (a int)",
                "CREATE FULLTEXT INDEX ON t (a) KEY INDEX pk",
            ],
        ] {
            // Each statement keeps its `;` from the splitter on SQL Server,
            // which must not change the decision.
            let terminated: Vec<String> = script.iter().map(|s| format!("{s};")).collect();
            for script in [stmts(&script), terminated] {
                assert!(
                    !wraps(Dialect::SqlServer, &script),
                    "{script:?} can't run in a transaction on SQL Server"
                );
            }
        }
        // A script that holds its own transaction is not wrapped in hubro's,
        // with or without the terminators the splitter keeps.
        for script in [
            stmts(&["BEGIN TRAN", "INSERT INTO t VALUES (1)", "COMMIT"]),
            stmts(&["BEGIN TRAN;", "INSERT INTO t VALUES (1);", "COMMIT;"]),
        ] {
            assert!(!wraps(Dialect::SqlServer, &script), "{script:?}");
        }
        // The same statements don't decline wrapping for the wrong dialect
        // reasons on SQL Server: ordinary DDL/DML still wraps.
        assert!(wraps(
            Dialect::SqlServer,
            &stmts(&["CREATE TABLE t (a int)", "INSERT INTO t VALUES (1)"])
        ));
        // Keywords inside strings must not trip the check.
        assert!(wraps(
            Dialect::SqlServer,
            &stmts(&[
                "INSERT INTO t VALUES ('backup this later')",
                "UPDATE t SET a = 1",
            ])
        ));
        // Postgres-only exclusions don't leak into SQL Server scripts
        // (CONCURRENTLY is not a T-SQL concept).
        assert!(wraps(
            Dialect::SqlServer,
            &stmts(&["SELECT 1", "DROP INDEX CONCURRENTLY i"])
        ));
        // And the SQL Server set doesn't leak into SQLite/Postgres. (CREATE
        // DATABASE is already non-transactional on Postgres; BACKUP is the
        // discriminating case.)
        for dialect in [Dialect::Sqlite, Dialect::Postgres] {
            assert!(wraps(
                dialect,
                &stmts(&["SELECT 1", "BACKUP DATABASE db TO DISK = 'x.bak'"])
            ));
        }
    }

    #[test]
    fn sqlite_value_setting_pragmas_prevent_wrapping() {
        // Setting PRAGMAs are silently ignored inside a transaction, so a
        // script containing one must run sequentially to take effect.
        for script in [
            vec!["PRAGMA foreign_keys = OFF", "DELETE FROM t"],
            vec!["PRAGMA journal_mode = WAL", "INSERT INTO t VALUES (1)"],
            vec!["PRAGMA foreign_keys(0)", "DELETE FROM t"],
        ] {
            assert!(
                !wraps(Dialect::Sqlite, &stmts(&script)),
                "{script:?} should run sequentially so the PRAGMA applies"
            );
        }
        // A bare read PRAGMA (no value) is transaction-safe, so it still wraps.
        assert!(wraps(
            Dialect::Sqlite,
            &stmts(&["PRAGMA user_version", "INSERT INTO t VALUES (1)"])
        ));
    }

    #[test]
    fn sqlserver_statements_that_must_lead_a_batch_are_recognized() {
        for sql in [
            "CREATE PROCEDURE p AS SELECT 1",
            "create proc dbo.p as begin select 1; end",
            "CREATE OR ALTER PROCEDURE p AS SELECT 1",
            "ALTER PROCEDURE p AS SELECT 1",
            "CREATE FUNCTION f() RETURNS int AS BEGIN RETURN 1 END",
            "CREATE TRIGGER tr ON t AFTER INSERT AS SELECT 1",
            "CREATE OR ALTER VIEW v AS SELECT 1 AS a",
            "ALTER VIEW v AS SELECT 1 AS a",
            "CREATE SCHEMA s",
            "CREATE DEFAULT d AS 0",
            "CREATE RULE r AS @x > 0",
            "-- leading comment\n/* and another */ CREATE VIEW v AS SELECT 1 AS a",
        ] {
            assert!(tsql_leads_own_batch(sql), "{sql:?}");
        }
        for sql in [
            "CREATE TABLE t (a int)",
            "CREATE TYPE ty FROM int",
            "CREATE INDEX ix ON t (a)",
            "ALTER TABLE t ADD b int",
            "ALTER SCHEMA s TRANSFER dbo.t",
            "DROP PROCEDURE p",
            "EXEC('CREATE VIEW v AS SELECT 1 AS a')",
            "SELECT 'CREATE VIEW v'",
            "-- CREATE VIEW v\nSELECT 1",
            "",
        ] {
            assert!(!tsql_leads_own_batch(sql), "{sql:?}");
        }
    }

    #[test]
    fn sqlserver_transaction_control_is_recognized_and_nothing_else() {
        // Sent as a plain batch, where a SET would outlive the statement — so
        // only bare transaction control, never a statement with room for one.
        for sql in [
            "BEGIN TRAN",
            "begin transaction",
            "BEGIN TRANSACTION load_rows",
            "BEGIN DISTRIBUTED TRAN",
            "COMMIT",
            "COMMIT TRAN",
            "COMMIT WORK",
            "ROLLBACK",
            "ROLLBACK TRANSACTION",
            "/* lead */ ROLLBACK TRAN -- done",
        ] {
            // A SQL Server statement keeps its `;` from the splitter, so each
            // case must route the same with one.
            for sql in [sql.to_string(), format!("{sql};")] {
                assert!(tsql_is_transaction_control(&sql), "{sql:?}");
            }
        }
        for sql in [
            // A name slot holding a statement: measured, a SET inside EXEC()
            // reverts when the EXEC returns, but nothing here depends on it.
            "BEGIN TRAN EXEC('SET NOCOUNT ON')",
            "BEGIN TRAN SETUSER 'guest'",
            "COMMIT TRAN EXECUTE('SET NOCOUNT ON')",
            "BEGIN TRAN SET NOCOUNT ON",
            "BEGIN TRAN\nSET NOCOUNT ON",
            "BEGIN TRY SELECT 1 END TRY BEGIN CATCH SELECT 2 END CATCH",
            "BEGIN SELECT 1 END",
            "COMMIT SELECT 5",
            "SAVE TRAN point",
            "SELECT 'BEGIN TRAN'",
            "-- BEGIN TRAN\nSELECT 1",
            "INSERT INTO t VALUES (1)",
        ] {
            for sql in [sql.to_string(), format!("{sql};")] {
                assert!(!tsql_is_transaction_control(&sql), "{sql:?}");
            }
        }
    }

    #[test]
    fn sqlserver_statements_that_create_a_temp_table_are_recognized() {
        for sql in [
            "CREATE TABLE #t (a int)",
            "create table #t (a int)",
            "CREATE TABLE ##global (a int)",
            "CREATE TABLE [#t] (a int)",
            "CREATE TABLE \"#t\" (a int)",
            "CREATE TABLE tempdb..#t (a int)",
            "CREATE TABLE tempdb.dbo.#t (a int)",
            "CREATE /* c */ TABLE -- c\n#t (a int)",
            "SELECT 1 AS a INTO #t",
            "SELECT * INTO [#t] FROM dbo.src",
            "WITH c AS (SELECT 1 AS a) SELECT a INTO #t FROM c",
            "IF OBJECT_ID('tempdb..#t') IS NULL CREATE TABLE #t (a int)",
            "IF 1 = 1 BEGIN CREATE TABLE #t (a int) END",
            "BEGIN TRY CREATE TABLE #t (a int) END TRY BEGIN CATCH END CATCH",
            "CREATE TABLE #t (a int) CREATE INDEX ix ON #t (a)",
            "SELECT 1 AS a INTO #t SELECT 2 AS b INTO #u",
        ] {
            assert!(tsql_creates_temp_table(sql), "{sql:?}");
        }
        for sql in [
            // No temp table created.
            "CREATE TABLE t (a int)",
            "CREATE TABLE dbo.t (a int)",
            "CREATE TABLE [t#] (a int)",
            "SELECT a INTO t FROM #src",
            "SELECT * FROM #t",
            "DROP TABLE #t",
            "ALTER TABLE #t ADD b int",
            "CREATE INDEX ix ON #t (a)",
            "DECLARE @t TABLE (a int)",
            "FETCH NEXT FROM c INTO @a",
            "SELECT 'CREATE TABLE #t' AS a",
            "-- CREATE TABLE #t\nSELECT 1",
            "CREATE TABLE t (a int) /* INTO #t */",
            // A write, which would report no rows as a plain batch.
            "INSERT INTO #t VALUES (1)",
            "CREATE TABLE #t (a int) INSERT INTO #t VALUES (1)",
            "SELECT 1 AS a INTO #t UPDATE #t SET a = 2",
            "SELECT 1 AS a INTO #t DELETE FROM #t",
            "SELECT 1 AS a INTO #t MERGE #t AS d USING #t AS s ON 1 = 0 \
             WHEN NOT MATCHED THEN INSERT (a) VALUES (1);",
            "SELECT 1 AS a INTO #t TRUNCATE TABLE #t",
            // Rows filled through `INTO` that the count reported: into a
            // permanent table, or by a statement that is not a read.
            "CREATE TABLE #a (a int) SELECT * INTO dbo.p FROM sys.objects",
            "SELECT 1 AS a INTO #t SELECT 2 AS b INTO dbo.p",
            "CREATE TABLE #a (a int) SELECT 1 AS b INTO #b",
            "DECLARE @n int = 1 SELECT @n AS n INTO #t",
            "IF 1 = 1 SELECT 1 AS a INTO #t",
            "CREATE TABLE #t (a int) EXEC dbo.p",
            "CREATE TABLE #t (a int) EXECUTE ('SELECT 1')",
            // A setting that would outlive the statement.
            "CREATE TABLE #t (a int) SET NOCOUNT ON",
            "SET NOEXEC ON CREATE TABLE #t (a int)",
            "CREATE TABLE #t (a int)\nset xact_abort on",
            "SELECT 1 AS a INTO #t SET TRANSACTION ISOLATION LEVEL SERIALIZABLE",
            // Transaction control, which error 266 must still stop.
            "CREATE TABLE #t (a int) COMMIT",
            "CREATE TABLE #t (a int) ROLLBACK",
            "BEGIN TRAN CREATE TABLE #t (a int)",
            "BEGIN TRANSACTION CREATE TABLE #t (a int)",
            "BEGIN DISTRIBUTED TRAN CREATE TABLE #t (a int)",
            "CREATE TABLE #t (a int) SAVE TRAN s",
            "IF 1 = 1 COMMIT WORK SELECT 1 AS a INTO #t",
        ] {
            assert!(!tsql_creates_temp_table(sql), "{sql:?}");
        }
    }

    #[test]
    fn a_temp_table_is_created_by_a_plain_batch_only_on_a_runs_own_connection() {
        // Every case runs again with the `;` a SQL Server statement keeps from
        // the splitter: a terminator must never change a route.
        let terminated = |sql: &str| [sql.to_string(), format!("{sql};")];
        for creates in terminated("CREATE TABLE #t (a int)") {
            // A checked batch, and only on a run's own connection.
            assert_eq!(
                tsql_route(&creates, true),
                TsqlRoute::CheckedBatch,
                "{creates:?}"
            );
            assert_eq!(tsql_route(&creates, false), TsqlRoute::Rpc, "{creates:?}");
        }
        // What must be a plain batch is one wherever it runs.
        for sql in ["CREATE VIEW v AS SELECT 1 AS a", "BEGIN TRAN", "COMMIT"] {
            for sql in terminated(sql) {
                assert_eq!(tsql_route(&sql, true), TsqlRoute::Batch, "{sql:?}");
                assert_eq!(tsql_route(&sql, false), TsqlRoute::Batch, "{sql:?}");
            }
        }
        for sql in [
            "INSERT INTO #t VALUES (1)",
            "SELECT * FROM #t",
            "SET NOCOUNT ON",
        ] {
            for sql in terminated(sql) {
                assert_eq!(tsql_route(&sql, true), TsqlRoute::Rpc, "{sql:?}");
                assert_eq!(tsql_route(&sql, false), TsqlRoute::Rpc, "{sql:?}");
            }
        }
    }

    /// The four ways a sequential run can end, decided over plain data: what
    /// the statements did, and whether ending the run rolled back a
    /// transaction the script had left open.
    #[test]
    fn a_sequential_run_reports_the_transaction_it_rolled_back() {
        let script = stmts(&["BEGIN", "INSERT INTO t VALUES (1)", "SELECT 1"]);
        let full = Capabilities::FULL;
        let outcome = |failure, ended| {
            let ran = match &failure {
                Some((i, _)) => &script[..=*i],
                None => &script[..],
            };
            sequential_outcome(full, Dialect::Postgres, &script, ran, failure, ended)
        };
        let failed = || Some((1, DbError::Query("boom".into())));
        let open = OpenTransaction {
            schema_changes_kept: false,
        };

        assert_eq!(outcome(None, Ok(false)), Ok(ScriptDone::default()));
        assert_eq!(
            outcome(None, Ok(true)),
            Ok(ScriptDone {
                rolled_back: Some(open),
                connection_closed: false,
            })
        );
        assert_eq!(
            outcome(failed(), Ok(true)).unwrap_err().rollback,
            Rollback::OpenTransaction(open)
        );
        let err = outcome(failed(), Ok(false)).unwrap_err();
        assert_eq!((err.statement_index, err.rollback), (1, Rollback::None));
        // A failed end after a failed statement: the statement's error stands,
        // and the rollback says only that the connection was closed.
        let err = outcome(failed(), Err(DbError::Query("gone".into()))).unwrap_err();
        assert_eq!(err.error, DbError::Query("boom".into()));
        assert_eq!(err.rollback, Rollback::ConnectionClosed);
        // A failed end after a clean run: every statement ran, so none is
        // blamed, but the run says the connection was closed rather than
        // claim nothing was lost.
        assert_eq!(
            outcome(None, Err(DbError::Query("gone".into()))),
            Ok(ScriptDone {
                rolled_back: None,
                connection_closed: true,
            })
        );
    }

    #[test]
    fn a_rolled_back_transaction_keeps_schema_changes_only_where_the_engine_does() {
        let no_ddl_rollback = Capabilities {
            transactional_ddl: false,
            ..Capabilities::FULL
        };
        let kept = |caps, ran: &[&str]| {
            let ran = stmts(ran);
            match sequential_outcome(caps, Dialect::Postgres, &ran, &ran, None, Ok(true)) {
                Ok(ScriptDone {
                    rolled_back: Some(open),
                    ..
                }) => open.schema_changes_kept,
                other => panic!("expected a rolled-back transaction, got {other:?}"),
            }
        };
        let with_ddl = [
            "BEGIN",
            "CREATE TABLE t (a int)",
            "INSERT INTO t VALUES (1)",
        ];
        let without = ["BEGIN", "INSERT INTO t VALUES (1)"];
        assert!(kept(no_ddl_rollback, &with_ddl));
        assert!(!kept(no_ddl_rollback, &without));
        assert!(!kept(Capabilities::FULL, &with_ddl));
    }

    #[test]
    fn only_a_read_that_cannot_touch_the_transaction_or_write_may_be_cut_off() {
        let pg = |sql| final_read_may_close(sql, Dialect::Postgres);
        // A SQL Server statement keeps its `;` from the splitter, so each case
        // there must decide the same with one.
        let ms = |sql: &str| {
            let plain = final_read_may_close(sql, Dialect::SqlServer);
            let terminated = final_read_may_close(&format!("{sql};"), Dialect::SqlServer);
            assert_eq!(plain, terminated, "{sql:?} with and without its `;`");
            plain
        };
        for sql in [
            "SELECT * FROM big",
            "with t as (select 1) select * from t",
            "VALUES (1), (2)",
            "TABLE big",
            "EXPLAIN SELECT 1",
            "SELECT f()",
        ] {
            assert!(pg(sql), "{sql:?}");
        }
        for sql in [
            "SHOW COMMIT TIMESTAMP",
            "SHOW search_path",
            "WITH w AS (INSERT INTO t SELECT 1 RETURNING a) SELECT a FROM w",
            "EXPLAIN ANALYZE DELETE FROM t",
            "SELECT * INTO copy FROM t",
            "PRAGMA table_info(t)",
        ] {
            assert!(!pg(sql), "{sql:?}");
        }
        for sql in [
            "SELECT * FROM big",
            "SELECT TOP (5) a FROM t",
            "WITH c AS (SELECT 1 AS a) SELECT a FROM c",
        ] {
            assert!(ms(sql), "{sql:?}");
        }
        for sql in [
            "SELECT 1 AS a COMMIT SELECT 2 AS b",
            "SELECT 1 AS a BEGIN TRAN INSERT INTO t VALUES (2) SELECT 2 AS b",
            "SELECT 1 AS a ROLLBACK",
            "SELECT 1 SAVE TRAN p",
            "SET IMPLICIT_TRANSACTIONS ON SELECT 1",
            "SET ANSI_DEFAULTS ON SELECT 1",
            "SELECT 1 EXEC dbo.p",
            "WITH c AS (SELECT 1 AS a) INSERT INTO t OUTPUT inserted.a SELECT a FROM c",
            "SELECT TOP (5) a FROM t INSERT INTO t VALUES (1)",
        ] {
            assert!(!ms(sql), "{sql:?}");
        }
        assert!(!final_read_may_close("SELECT 1", Dialect::Sqlite));
    }

    #[test]
    fn preview_truncates_multibyte_text_on_char_boundaries() {
        let long = format!("SELECT '{}'", "ы".repeat(100));
        let preview = statement_preview(&long);
        assert_eq!(preview.chars().count(), 61);
        assert!(preview.ends_with('…'));
    }
}

/// The engine-side backstop of a read-only connection, driven **past the
/// gate**: [`run_script_read_only`] is handed statements the gate would have
/// refused, standing in for a write hidden from it by a lexer mismatch. The
/// public-API half — writes the gate genuinely misses, sent through
/// [`run_script`] — is `tests/db_read_only.rs`.
///
/// The SQLite half always runs. The Postgres-wire half runs once per engine
/// whose `HUBRO_*_TEST_URL` is set (`tests/CLAUDE.md`), against fixtures named
/// for this process so it does not collide with the engine's own suite.
#[cfg(test)]
mod read_only_backstop {
    use super::*;
    use crate::db::caps::WriteProtection;
    use crate::db::plan::explain_statement;
    use crate::db::value::Value;

    #[test]
    fn only_a_connection_that_may_not_write_is_enforced_by_the_engine() {
        assert!(!engine_enforces_read_only(Capabilities::FULL));
        assert!(engine_enforces_read_only(Capabilities::FULL.read_only()));
        // The user's marking, as the run path receives it.
        for (protection, enforced) in [
            (WriteProtection::Open, false),
            (WriteProtection::Confirm, false),
            (WriteProtection::ReadOnly, true),
        ] {
            assert_eq!(
                engine_enforces_read_only(protection.apply(Capabilities::FULL)),
                enforced,
                "{protection:?}"
            );
        }
        // A backend without transactions (RisingWave) is still enforced
        // when marked: its read-only transaction is the one kind it has.
        let no_tx = Capabilities {
            transactions: false,
            transactional_ddl: false,
            ..Capabilities::FULL
        };
        assert!(engine_enforces_read_only(
            WriteProtection::ReadOnly.apply(no_tx)
        ));
    }

    fn stmts(sql: &[&str]) -> Vec<String> {
        sql.iter().map(|s| s.to_string()).collect()
    }

    /// Runs `statements` through the read-only path with the gate bypassed,
    /// returning the outcome and the rows each read produced.
    async fn run_past_the_gate(
        pool: &DbPool,
        statements: &[&str],
    ) -> (Result<(), ScriptError>, Vec<StatementOutcome>) {
        let run = pool
            .begin_read_only_run()
            .await
            .expect("a read-only run opens")
            .expect("this backend has a read-only mode");
        let mut outcomes = Vec::new();
        let result = run_script_read_only(run, pool.dialect(), &stmts(statements), |r| {
            outcomes.push(r.outcome)
        })
        .await;
        (result, outcomes)
    }

    async fn scalar(pool: &DbPool, sql: &str) -> Value {
        let result = pool
            .query_unbounded(sql)
            .await
            .unwrap_or_else(|e| panic!("{sql}: {e}"));
        result.rows[0][0].clone()
    }

    async fn sqlite_pool(path: &std::path::Path) -> DbPool {
        DbPool::open_sqlite(path).await.unwrap()
    }

    async fn sqlite_fixture(dir: &tempfile::TempDir, name: &str, wal: bool) -> std::path::PathBuf {
        let path = dir.path().join(name);
        let options = sqlx::sqlite::SqliteConnectOptions::new()
            .filename(&path)
            .create_if_missing(true)
            .journal_mode(if wal {
                sqlx::sqlite::SqliteJournalMode::Wal
            } else {
                sqlx::sqlite::SqliteJournalMode::Delete
            });
        let pool = sqlx::SqlitePool::connect_with(options).await.unwrap();
        sqlx::query("CREATE TABLE t (a INTEGER)")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO t VALUES (1)")
            .execute(&pool)
            .await
            .unwrap();
        pool.close().await;
        path
    }

    /// The gate's two sides for a SQLite connection marked read-only, through
    /// the real [`run_script`]: a reading call-form pragma and a read calling
    /// `replace()` run, and a setting pragma in the same call form is refused
    /// by the gate — and, past the gate, by the engine, which is the
    /// measurement that keeps it on the charged side.
    #[tokio::test]
    async fn a_marked_sqlite_connection_runs_reading_pragmas_and_refuses_setting_ones() {
        let dir = tempfile::tempdir().unwrap();
        let path = sqlite_fixture(&dir, "main.db", false).await;
        let pool = sqlite_pool(&path).await;
        let marked = WriteProtection::ReadOnly.apply(Capabilities::FULL);

        let mut outcomes = Vec::new();
        run_script(
            &pool,
            marked,
            &stmts(&[
                "PRAGMA table_info(t)",
                "WITH x AS (SELECT replace('a-b', '-', ' ') AS n) SELECT n FROM x",
            ]),
            |r| outcomes.push(r.outcome),
        )
        .await
        .unwrap_or_else(|e| panic!("a read was refused: {e:?}"));
        assert!(
            matches!(&outcomes[0], StatementOutcome::Rows(r) if r.rows.len() == 1 && r.rows[0][1] == Value::Text("a".into())),
            "{outcomes:?}"
        );
        assert!(
            matches!(&outcomes[1], StatementOutcome::Rows(r) if r.rows == vec![vec![Value::Text("a b".into())]]),
            "{outcomes:?}"
        );

        let refused = run_script(&pool, marked, &stmts(&["PRAGMA user_version(5)"]), |_| {
            panic!("a refused script must not execute a statement")
        })
        .await
        .expect_err("a setting pragma's call form must be refused");
        assert_eq!(
            refused.error,
            DbError::Unsupported(caps::NO_MUTATE.to_string())
        );
        let (result, _) = run_past_the_gate(&pool, &["PRAGMA user_version(5)"]).await;
        assert!(
            result.is_err(),
            "the engine let PRAGMA user_version(5) write"
        );
        assert_eq!(
            scalar(&pool, "PRAGMA user_version").await,
            Value::Integer(0)
        );

        // Unmarked, the same call form really does write: what the refusal
        // above is protecting.
        run_script(
            &pool,
            Capabilities::FULL,
            &stmts(&["PRAGMA user_version(5)"]),
            |_| {},
        )
        .await
        .unwrap();
        assert_eq!(
            scalar(&pool, "PRAGMA user_version").await,
            Value::Integer(5)
        );
        pool.close().await;
    }

    #[tokio::test]
    async fn sqlite_refuses_writes_the_gate_never_saw() {
        for wal in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let path = sqlite_fixture(&dir, "main.db", wal).await;
            let other = sqlite_fixture(&dir, "other.db", false).await;
            let pool = sqlite_pool(&path).await;

            let attacks: &[&[&str]] = &[
                &["DELETE FROM t"],
                &["INSERT INTO t VALUES (2)"],
                &["CREATE TABLE t2 (a)"],
                &["DROP TABLE t"],
                &["PRAGMA user_version = 7"],
                // sqlx runs every statement in the string it is handed, so a
                // mismatch can carry the pragma that would undo `query_only`.
                &["PRAGMA query_only = 0; DELETE FROM t"],
                &["PRAGMA query_only = 0", "DELETE FROM t"],
            ];
            for attack in attacks {
                let (result, _) = run_past_the_gate(&pool, attack).await;
                assert!(result.is_err(), "wal={wal}: {attack:?} ran");
            }
            // ATTACH inherits the read-only flag; asking for more is refused.
            let attach = format!("ATTACH '{}' AS o", other.display());
            let (result, _) =
                run_past_the_gate(&pool, &[&attach, "INSERT INTO o.t VALUES (9)"]).await;
            assert!(result.is_err(), "wal={wal}: wrote through ATTACH");
            let attach_rw = format!("ATTACH 'file:{}?mode=rw' AS o", other.display());
            let (result, _) = run_past_the_gate(&pool, &[&attach_rw]).await;
            assert!(result.is_err(), "wal={wal}: ATTACH widened to read-write");

            assert_eq!(
                scalar(&pool, "SELECT count(*) FROM t").await,
                Value::Integer(1)
            );
            assert_eq!(scalar(&pool, "SELECT a FROM t").await, Value::Integer(1));
            assert_eq!(
                scalar(
                    &pool,
                    "SELECT count(*) FROM sqlite_master WHERE name = 't2'"
                )
                .await,
                Value::Integer(0)
            );
            assert_eq!(
                scalar(&pool, "PRAGMA user_version").await,
                Value::Integer(0)
            );
            let other_pool = sqlite_pool(&other).await;
            assert_eq!(
                scalar(&other_pool, "SELECT count(*) FROM t").await,
                Value::Integer(1)
            );
            other_pool.close().await;

            // Reads still work, through the same path.
            let explain = explain_statement(
                "SELECT * FROM t",
                pool.explain_support().expect("SQLite explains"),
            );
            let (result, outcomes) = run_past_the_gate(
                &pool,
                &["SELECT a FROM t", "PRAGMA table_info(t)", &explain],
            )
            .await;
            result.unwrap_or_else(|e| panic!("wal={wal}: a read failed: {e:?}"));
            assert_eq!(outcomes.len(), 3);
            assert!(
                matches!(&outcomes[0], StatementOutcome::Rows(r) if r.rows == vec![vec![Value::Integer(1)]])
            );

            // And the pool itself still writes: the marking lives in the run,
            // not in the connection the rest of the app uses.
            pool.execute("INSERT INTO t VALUES (3)").await.unwrap();
            pool.close().await;
        }
    }

    /// Every Postgres-wire engine whose URL is set, with its name.
    fn pg_wire_urls() -> Vec<(&'static str, String)> {
        [
            "HUBRO_PG_TEST_URL",
            "HUBRO_TIMESCALE_TEST_URL",
            "HUBRO_CITUS_TEST_URL",
            "HUBRO_CRDB_TEST_URL",
            "HUBRO_YUGABYTE_TEST_URL",
            "HUBRO_MATERIALIZE_TEST_URL",
            "HUBRO_RISINGWAVE_TEST_URL",
        ]
        .into_iter()
        .filter_map(|var| std::env::var(var).ok().map(|url| (var, url)))
        .collect()
    }

    /// RisingWave applies an `INSERT` asynchronously: a row count read straight
    /// after one can miss it, which would make a write that went through look
    /// refused.
    async fn settle(pool: &DbPool) {
        if pool.pg_flavor() == Some(crate::db::postgres::PgFlavor::RisingWave) {
            pool.execute("FLUSH").await.unwrap();
        }
    }

    async fn table_exists(pool: &DbPool, name: &str) -> bool {
        let sql =
            format!("SELECT count(*) FROM information_schema.tables WHERE table_name = '{name}'");
        scalar(pool, &sql).await != Value::Integer(0)
    }

    #[tokio::test]
    async fn postgres_wire_engines_refuse_writes_the_gate_never_saw() {
        let pid = std::process::id();
        // Collected rather than panicking at the first, so one run reports
        // every engine a regression reaches — the engines differ in exactly
        // the ways this test exists for.
        let mut failures = Vec::new();
        for (var, url) in pg_wire_urls() {
            let pool = DbPool::open_postgres(&url)
                .await
                .unwrap_or_else(|e| panic!("{var}: {e}"));
            let t = format!("hubro_ro_t_{pid}");
            let made = format!("hubro_ro_made_{pid}");
            for name in [&t, &made] {
                let _ = pool.execute(&format!("DROP TABLE IF EXISTS {name}")).await;
            }
            pool.execute(&format!("CREATE TABLE {t} (a int)"))
                .await
                .unwrap();

            let insert = format!("INSERT INTO {t} VALUES (1)");
            let create = format!("CREATE TABLE {made} (a int)");
            let drop = format!("DROP TABLE {t}");
            // Each attack is one run; a run stops at its first failure, so
            // the statements after a refused one never reach the server —
            // which is why the effects are checked below rather than the
            // error of each statement.
            let commit_then_insert = format!("COMMIT; {insert}");
            let attacks: Vec<Vec<&str>> = vec![
                vec![&insert],
                vec![&create],
                vec![&drop],
                // Hidden behind a mismatch, two commands arrive as one
                // statement; the extended protocol refuses the pair.
                vec![&commit_then_insert],
                // A statement can end or reopen hubro's transaction, but the
                // next statement gets a new one.
                vec!["COMMIT", &insert],
                vec!["ROLLBACK", &create],
                vec!["SET TRANSACTION READ WRITE", &insert],
                vec!["BEGIN READ WRITE", &insert],
                vec![
                    "SET SESSION CHARACTERISTICS AS TRANSACTION READ WRITE",
                    &insert,
                ],
                vec!["SET default_transaction_read_only = off", &insert],
                vec!["RESET ALL", &create],
                // CockroachDB: DDL leaves the transaction unless hubro's
                // `SET LOCAL` holds it there, so turning the setting back on
                // for the session must not help.
                vec!["SET autocommit_before_ddl = on", &create],
            ];
            for attack in &attacks {
                let (_, _) = run_past_the_gate(&pool, attack).await;
            }
            // The plain INSERT must be refused outright, everywhere.
            let (result, _) = run_past_the_gate(&pool, &[&insert]).await;
            match result {
                Err(ScriptError {
                    error: DbError::Query(_),
                    ..
                }) => {}
                other => failures.push(format!("{var}: INSERT was not refused: {other:?}")),
            }

            settle(&pool).await;
            if !table_exists(&pool, &t).await {
                failures.push(format!("{var}: DROP TABLE went through"));
                pool.execute(&format!("CREATE TABLE {t} (a int)"))
                    .await
                    .unwrap();
            }
            if table_exists(&pool, &made).await {
                failures.push(format!("{var}: CREATE TABLE went through"));
                pool.execute(&format!("DROP TABLE {made}")).await.unwrap();
            }
            let rows = scalar(&pool, &format!("SELECT count(*) FROM {t}")).await;
            if rows != Value::Integer(0) {
                failures.push(format!("{var}: a row was written ({rows:?})"));
            }

            // Reads still work through the same path: a table, the catalog,
            // and the plan view's own spelling of EXPLAIN.
            let select = format!("SELECT count(*) FROM {t}");
            let explain = explain_statement(
                &format!("SELECT * FROM {t}"),
                pool.explain_support()
                    .expect("every Postgres-wire engine explains"),
            );
            let (result, outcomes) = run_past_the_gate(
                &pool,
                &[
                    &select,
                    "SELECT count(*) FROM information_schema.columns",
                    &explain,
                ],
            )
            .await;
            if let Err(e) = result {
                failures.push(format!("{var}: a read failed: {e:?}"));
            } else {
                assert_eq!(outcomes.len(), 3, "{var}");
            }

            // Nothing leaked into the pool: the same connections still write.
            pool.execute(&insert)
                .await
                .unwrap_or_else(|e| panic!("{var}: the pool stopped writing: {e}"));
            pool.execute(&format!("DROP TABLE {t}")).await.unwrap();
            pool.close().await;
        }
        assert!(failures.is_empty(), "{failures:#?}");
    }
}

/// The statement loop every script run shares ([`run_statements`]) and the
/// per-mode decisions around it, executed over a fake hold rather than read
/// off the source: what runs after a failure, whether the hold is told a
/// statement failed, which statement lets go of the hold, and what each mode
/// reports.
#[cfg(test)]
mod statement_loop {
    use super::*;
    use std::cell::RefCell;
    use std::rc::Rc;

    /// A hold that logs what the loop asks of it and fails any statement
    /// naming `FAIL`. Like `ScriptTx`, its last read is an ordinary one,
    /// reported before the hold is let go of.
    struct Fake(Rc<RefCell<Vec<String>>>);

    impl StatementExecutor for Fake {
        type Ended = bool;
        const ENDS_WITH_FINAL_READ: bool = false;

        async fn read(&mut self, sql: &str, _: u64, _: usize) -> Result<Capped, DbError> {
            self.0.borrow_mut().push(format!("read {sql}"));
            if sql.contains("FAIL") {
                return Err(DbError::Query(sql.to_string()));
            }
            Ok(Capped::from((QueryResult::default(), false)))
        }

        async fn write(&mut self, sql: &str) -> Result<RowsAffected, DbError> {
            self.0.borrow_mut().push(format!("write {sql}"));
            if sql.contains("FAIL") {
                return Err(DbError::Query(sql.to_string()));
            }
            Ok(RowsAffected::Exact(1))
        }

        /// Ended is the `failed` it was told.
        async fn end(self, failed: bool) -> bool {
            self.0.borrow_mut().push(format!("end failed={failed}"));
            failed
        }

        async fn finish_with_read(
            self,
            _: &str,
            _: u64,
            _: usize,
        ) -> (Result<Capped, DbError>, bool) {
            unreachable!("Fake does not end with its final read")
        }
    }

    /// [`Fake`] that lets go of the hold with its final read, as
    /// `ScriptConn` and `ReadOnlyRun` do.
    struct Closing(Fake);

    impl StatementExecutor for Closing {
        type Ended = bool;
        const ENDS_WITH_FINAL_READ: bool = true;

        async fn read(&mut self, sql: &str, rows: u64, cap: usize) -> Result<Capped, DbError> {
            self.0.read(sql, rows, cap).await
        }

        async fn write(&mut self, sql: &str) -> Result<RowsAffected, DbError> {
            self.0.write(sql).await
        }

        async fn end(self, failed: bool) -> bool {
            self.0.end(failed).await
        }

        async fn finish_with_read(
            self,
            sql: &str,
            _: u64,
            _: usize,
        ) -> (Result<Capped, DbError>, bool) {
            self.0 .0.borrow_mut().push(format!("finish {sql}"));
            (Ok(Capped::from((QueryResult::default(), false))), false)
        }
    }

    fn stmts(sql: &[&str]) -> Vec<String> {
        sql.iter().map(|s| s.to_string()).collect()
    }

    /// Runs `sql` through the loop on a [`Fake`] (or a [`Closing`] one),
    /// returning the failure's index, what `end` was told, and one log of
    /// what the hold was asked and what was reported (`report <statement>`),
    /// in the order they happened.
    async fn drive(sql: &[&str], closing: bool) -> (Option<usize>, bool, Vec<String>) {
        let log = Rc::new(RefCell::new(Vec::new()));
        let statements = stmts(sql);
        let report = {
            let log = log.clone();
            move |r: StatementResult| log.borrow_mut().push(format!("report {}", r.preview))
        };
        let (failure, ended) = if closing {
            run_statements(
                Closing(Fake(log.clone())),
                Dialect::Postgres,
                &statements,
                report,
            )
            .await
        } else {
            run_statements(Fake(log.clone()), Dialect::Postgres, &statements, report).await
        };
        let log = log.borrow().clone();
        (failure.map(|(at, _)| at), ended, log)
    }

    #[tokio::test]
    async fn nothing_runs_after_a_failed_statement_and_the_hold_is_told() {
        for closing in [false, true] {
            let (failed_at, ended, log) = drive(
                &[
                    "INSERT INTO t VALUES (1)",
                    "INSERT FAIL",
                    "INSERT INTO t VALUES (3)",
                ],
                closing,
            )
            .await;
            assert_eq!(failed_at, Some(1));
            assert!(ended, "the hold was not told a statement failed");
            assert_eq!(
                log,
                [
                    "write INSERT INTO t VALUES (1)",
                    "report INSERT INTO t VALUES (1)",
                    "write INSERT FAIL",
                    "end failed=true"
                ],
                "closing: {closing}"
            );
        }
    }

    #[tokio::test]
    async fn a_clean_run_lets_go_of_the_hold_as_unfailed_once() {
        let (failed_at, ended, log) = drive(
            &["INSERT INTO t VALUES (1)", "SELECT 1", "DELETE FROM t"],
            false,
        )
        .await;
        assert_eq!(failed_at, None);
        assert!(!ended);
        assert_eq!(
            log,
            [
                "write INSERT INTO t VALUES (1)",
                "report INSERT INTO t VALUES (1)",
                "read SELECT 1",
                "report SELECT 1",
                "write DELETE FROM t",
                "report DELETE FROM t",
                "end failed=false"
            ]
        );
    }

    /// A hold that lets go with its final read does so with the last
    /// statement, a read, and is not let go of a second time. One that does
    /// not — the atomic run — reports its last read **before** the hold is
    /// let go of: the commit comes after the result is shown.
    #[tokio::test]
    async fn the_final_read_is_reported_before_a_commit_and_ends_only_a_closing_hold() {
        let (_, _, log) = drive(&["SELECT 1", "SELECT 2"], true).await;
        assert_eq!(
            log,
            [
                "read SELECT 1",
                "report SELECT 1",
                "finish SELECT 2",
                "report SELECT 2"
            ]
        );
        let (_, _, log) = drive(&["SELECT 1", "SELECT 2"], false).await;
        assert_eq!(
            log,
            [
                "read SELECT 1",
                "report SELECT 1",
                "read SELECT 2",
                "report SELECT 2",
                "end failed=false"
            ]
        );
        let (failed_at, ended, log) = drive(&["SELECT 1", "SELECT FAIL"], false).await;
        assert_eq!(failed_at, Some(1));
        assert!(ended);
        assert_eq!(
            log,
            [
                "read SELECT 1",
                "report SELECT 1",
                "read SELECT FAIL",
                "end failed=true"
            ]
        );
        for (ends, reads, is_last, expected) in [
            (true, true, true, Step::FinishWithRead),
            (true, true, false, Step::Read),
            (true, false, true, Step::Write),
            (false, true, true, Step::Read),
            (false, false, true, Step::Write),
            (false, true, false, Step::Read),
        ] {
            assert_eq!(
                step(ends, reads, is_last),
                expected,
                "ends {ends}, reads {reads}, last {is_last}"
            );
        }
    }

    /// Which holds let go with their final read: the atomic run's does not,
    /// so its last result is reported before its commit.
    #[test]
    fn only_the_sequential_and_read_only_holds_end_with_their_final_read() {
        let (sequential, read_only, atomic) = (
            <ScriptConn as StatementExecutor>::ENDS_WITH_FINAL_READ,
            <ReadOnlyRun as StatementExecutor>::ENDS_WITH_FINAL_READ,
            <ScriptTx<'static> as StatementExecutor>::ENDS_WITH_FINAL_READ,
        );
        assert_eq!((sequential, read_only, atomic), (true, true, false));
    }

    #[test]
    fn an_atomic_run_commits_only_when_no_statement_failed() {
        assert!(tx_commits(false));
        assert!(!tx_commits(true));
    }

    /// What an atomic run reports: a failed statement is judged over what ran
    /// up to it, a failed commit over the whole script and labelled `COMMIT`.
    #[test]
    fn an_atomic_run_reports_a_failed_statement_and_a_failed_commit_apart() {
        let escapes = Capabilities {
            transactional_ddl: false,
            ..Capabilities::FULL
        };
        let script = stmts(&[
            "INSERT INTO t VALUES (1)",
            "UPDATE t SET a = 2",
            "CREATE TABLE u (a int)",
        ]);
        let failed = atomic_outcome(
            escapes,
            Dialect::Postgres,
            &script,
            Some((1, DbError::Query("boom".into()))),
            TxEnded::RolledBack(RolledBack::Own),
        )
        .unwrap_err();
        assert_eq!(failed.statement_index, 1);
        assert_eq!(failed.preview, statement_preview(&script[1]));
        assert_eq!(failed.rollback, Rollback::Full);
        let commit = atomic_outcome(
            escapes,
            Dialect::Postgres,
            &script,
            None,
            TxEnded::Committed(Err((DbError::Query("deferred".into()), RolledBack::Own))),
        )
        .unwrap_err();
        assert_eq!(commit.statement_index, 2);
        assert_eq!(commit.preview, "COMMIT");
        assert_eq!(commit.rollback, Rollback::ExceptSchemaChanges);
        assert_eq!(
            atomic_outcome(
                escapes,
                Dialect::Postgres,
                &script,
                None,
                TxEnded::Committed(Ok(()))
            ),
            Ok(())
        );
    }

    #[test]
    fn a_read_only_run_reports_its_failure_with_nothing_rolled_back() {
        let script = stmts(&["SELECT 1", "INSERT INTO t VALUES (1)"]);
        let failed =
            read_only_outcome(&script, Some((1, DbError::Query("read-only".into())))).unwrap_err();
        assert_eq!(failed.statement_index, 1);
        assert_eq!(failed.rollback, Rollback::None);
        assert_eq!(read_only_outcome(&script, None), Ok(()));
    }
}
