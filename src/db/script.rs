//! Multi-statement SQL scripts: splitting, classification, and execution.
//!
//! [`split_statements`] is a lexer-level splitter — it understands quotes,
//! comments, and dollar-quoting well enough to find statement boundaries,
//! without parsing SQL. [`run_script`] executes the statements one by one,
//! stopping at the first error. A multi-statement script runs **atomically**
//! in one transaction ([`wrap_atomically`]) — a mid-script failure rolls the
//! whole thing back — unless it manages transactions itself (`BEGIN`/`COMMIT`)
//! or contains a statement that can't run inside a transaction (e.g. `VACUUM`,
//! Postgres `CREATE INDEX CONCURRENTLY`), in which case it falls back to
//! sequential autocommit and earlier statements' effects persist.

use std::ops::Range;

use super::caps::{self, Capabilities};
use super::error::DbError;
use super::registry::{DbPool, MAX_QUERY_ROWS};
use super::sql::Dialect;
use super::value::QueryResult;

/// One lexed region of a SQL script. [`lex_regions`] is the single place
/// that understands quote/comment/bracket/dollar-quote state — the statement
/// splitter, the string/comment stripper, and the first-keyword scanner are
/// all thin consumers of it, so a lexing edge case fixed here is fixed in
/// all three at once (FRE-136).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RegionKind {
    /// Plain SQL: keywords, identifiers, operators, whitespace.
    Code,
    /// A single- or double-quoted string (`'…'`, `"…"`), quotes included.
    /// Doubled quotes need no special casing: `'it''s'` lexes as two
    /// adjacent String regions, which every consumer treats the same as
    /// one.
    String,
    /// A line comment (`-- …`, excluding the terminating newline — the
    /// splitter's GO line tracking must see that newline as code) or a
    /// block comment (`/* … */`, nesting like Postgres; SQLite never nests
    /// but treating `/*` inside a comment as nested is harmless there).
    Comment,
    /// A `[bracketed identifier]`, brackets included (`]]` is an escaped
    /// `]`), so `[a;b]` or `[a--b]` never split or start comments. Only
    /// produced for [`Dialect::SqlServer`]: SQLite also accepts
    /// `[brackets]` as a compat quirk, but this lexer has never bracketed
    /// them there and identifiers with `;`/`--` inside are vanishingly
    /// rare outside T-SQL scripts — bracket lexing is deliberately SQL
    /// Server-only, keeping SQLite/Postgres behavior unchanged.
    Bracket,
    /// A dollar-quoted string (`$$…$$`, `$tag$…$tag$` — Postgres),
    /// delimiters included. A bare `$` (e.g. a `$1` parameter placeholder)
    /// is code, not a delimiter.
    DollarQuote,
}

/// Lexes `sql` into contiguous regions covering every byte, in order.
///
/// Unterminated strings, comments, brackets, and dollar quotes swallow the
/// rest of the input as their region — the graceful degradation every
/// consumer wants. Known limitation: Postgres `E'…'` escape-string
/// backslash quoting is not understood (`E'\''` misparses); standard SQL
/// doubling (`''`) is fine.
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
fn region_at(sql: &str, i: usize, dialect: Dialect) -> Option<(usize, RegionKind)> {
    let bytes = sql.as_bytes();
    match bytes[i] {
        quote @ (b'\'' | b'"') => {
            let mut j = i + 1;
            while j < bytes.len() && bytes[j] != quote {
                j += 1;
            }
            // Past the closing quote; unterminated, the rest is the string.
            Some(((j + 1).min(bytes.len()), RegionKind::String))
        }
        b'[' if dialect == Dialect::SqlServer => {
            let mut j = i + 1;
            while j < bytes.len() {
                if bytes[j] == b']' {
                    if bytes.get(j + 1) == Some(&b']') {
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
        b'-' if bytes.get(i + 1) == Some(&b'-') => {
            let mut j = i + 2;
            while j < bytes.len() && bytes[j] != b'\n' {
                j += 1;
            }
            Some((j, RegionKind::Comment))
        }
        b'/' if bytes.get(i + 1) == Some(&b'*') => {
            let mut depth = 1usize;
            let mut j = i + 2;
            while j < bytes.len() && depth > 0 {
                if bytes[j] == b'/' && bytes.get(j + 1) == Some(&b'*') {
                    depth += 1;
                    j += 2;
                } else if bytes[j] == b'*' && bytes.get(j + 1) == Some(&b'/') {
                    depth -= 1;
                    j += 2;
                } else {
                    j += 1;
                }
            }
            Some((j, RegionKind::Comment))
        }
        b'$' => {
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

/// Splits a script into individual statements on `;`, respecting quoted
/// strings, comments, dollar quotes, and (on SQL Server) `[bracketed
/// identifiers]` — see [`lex_regions`] for the exact lexing rules and the
/// known `E'…'` limitation. A trailing statement without a semicolon is
/// kept; statements that are empty (only whitespace and/or comments) are
/// skipped.
///
/// On SQL Server, `GO` also separates statements. `GO` is a client-side
/// batch separator (SSMS/sqlcmd), not T-SQL: it only counts when it stands
/// alone on its own line (leading whitespace allowed), optionally followed
/// by a repeat count — `GO 5` is treated as a plain separator with the
/// count ignored (repeating a batch is a scripting-tool feature, not
/// something a viewer should replay). `GO` inside strings, comments, or
/// bracketed identifiers, or sharing a line with other code, never splits.
pub fn split_statements(sql: &str, dialect: Dialect) -> Vec<String> {
    let bytes = sql.as_bytes();
    let mut statements = Vec::new();
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
                            i = line_end;
                            continue;
                        }
                    }
                    match bytes[i] {
                        b';' => {
                            if significant {
                                statements.push(sql[start..i].trim().to_string());
                            }
                            start = i + 1;
                            significant = false;
                        }
                        b'\n' => line_start = i + 1,
                        other => {
                            if !other.is_ascii_whitespace() {
                                significant = true;
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
    if !matches!(bytes.get(j), Some(c) if c.is_ascii_alphabetic() || *c == b'_') {
        return None;
    }
    while matches!(bytes.get(j), Some(c) if c.is_ascii_alphanumeric() || *c == b'_') {
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
pub fn classify_statement(sql: &str) -> StatementKind {
    match first_keyword(sql).to_ascii_lowercase().as_str() {
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
const EMBEDDED_DML_KEYWORDS: [&str; 6] =
    ["insert", "update", "delete", "merge", "replace", "truncate"];
const EMBEDDED_DDL_KEYWORDS: [&str; 4] = ["create", "drop", "alter", "truncate"];

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
pub fn statement_needs(sql: &str, dialect: Dialect) -> StatementNeeds {
    // Stripped once here; every scan below works on the stripped form.
    // Before FRE-136 each helper re-stripped the same statement, up to ~6
    // times per capability check.
    let stripped = strip_strings_and_comments(sql, dialect);
    if !confirmation_needed(sql, &stripped) {
        return StatementNeeds::default();
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
    let mutate = has(&EMBEDDED_DML_KEYWORDS);
    let ddl = classify_statement(sql) == StatementKind::Ddl || has(&EMBEDDED_DDL_KEYWORDS);
    if !mutate && !ddl {
        // Bare transaction control (`BEGIN`, `COMMIT`, `ROLLBACK`, …) changes
        // nothing by itself, and the statements it brackets are each checked
        // on their own — so it needs nothing, rather than being refused with
        // a reason that doesn't describe it. This is only reached when no
        // write token appears anywhere in the statement, so a T-SQL
        // `BEGIN … DELETE … END` block is still charged for its DELETE.
        if manages_own_transaction(&stripped) {
            return StatementNeeds::default();
        }
        return StatementNeeds {
            mutate: true,
            ddl: true,
        };
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
/// - `SELECT`: an `INTO` token — `SELECT … INTO new_table` creates a table.
/// - `PRAGMA`: a `=` or `(` — the value-setting forms. This deliberately
///   over-prompts call-form read pragmas like `PRAGMA table_info(t)`: some
///   pragmas accept both spellings for setting, and prompting is the
///   fail-safe side.
///
/// The dialect matters on SQL Server, where `[bracketed identifiers]` are
/// lexed (see [`strip_strings_and_comments`]): a quote inside one
/// (`SELECT [o'brien] * INTO t2 FROM t`) must not invert string tracking
/// and hide the `INTO`, and `SELECT [into]` must not over-prompt.
pub fn needs_confirmation(sql: &str, dialect: Dialect) -> bool {
    confirmation_needed(sql, &strip_strings_and_comments(sql, dialect))
}

/// [`needs_confirmation`] against an already-stripped statement, so
/// [`statement_needs`] can strip once and share the result with its own
/// keyword scans.
fn confirmation_needed(sql: &str, stripped: &str) -> bool {
    if classify_statement(sql) != StatementKind::Read {
        return true;
    }
    match first_keyword(sql).to_ascii_lowercase().as_str() {
        "with" | "explain" => has_word(stripped, |word| {
            let word = word.to_ascii_lowercase();
            EMBEDDED_DML_KEYWORDS.contains(&word.as_str())
                || EMBEDDED_DDL_KEYWORDS.contains(&word.as_str())
                || word == "into"
        }),
        "select" => has_word(stripped, |word| word.eq_ignore_ascii_case("into")),
        "pragma" => stripped.contains('=') || stripped.contains('('),
        _ => false,
    }
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
/// region — quoted strings (single, double, dollar), comments, and (on SQL
/// Server) `[bracketed identifiers]` — blanked out to spaces, so token
/// scans can't be fooled by literals like `'please do not DELETE me'`, a
/// quote inside a bracketed identifier can't invert string tracking, and a
/// keyword inside one can't masquerade as a token. Brackets stay inert on
/// SQLite/Postgres, matching the splitter.
fn strip_strings_and_comments(sql: &str, dialect: Dialect) -> String {
    let bytes = sql.as_bytes();
    let mut out = vec![b' '; bytes.len()];
    for (range, kind) in lex_regions(sql, dialect) {
        if kind == RegionKind::Code {
            out[range.clone()].copy_from_slice(&bytes[range]);
        }
    }
    // Multibyte chars survive intact: regions always start and end at ASCII
    // delimiters (a UTF-8 continuation byte can never equal an ASCII
    // quote/comment marker), so a code region copied here is never split
    // mid-sequence.
    String::from_utf8_lossy(&out).into_owned()
}

/// The first keyword of a statement, skipping leading whitespace, comments,
/// and opening parentheses (`(SELECT …)` is a read). Anything else — a
/// string, a dollar quote, a non-keyword code byte — ends the scan: a
/// statement opening with one has no first keyword and classifies as a
/// write, the safe side.
///
/// Lexes without a dialect (as SQLite) because [`classify_statement`] has
/// none to pass, and the answer is dialect-independent anyway: a leading
/// `[` ends the scan with an empty keyword whether it opens a bracket
/// region or sits in code as a non-keyword byte.
fn first_keyword(sql: &str) -> &str {
    let bytes = sql.as_bytes();
    for (range, kind) in lex_regions(sql, Dialect::Sqlite) {
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
    /// A write: the driver's affected-row count.
    Affected(u64),
}

/// One executed statement: its preview (for the result header) plus its
/// outcome.
#[derive(Debug, Clone, PartialEq)]
pub struct StatementResult {
    pub preview: String,
    pub outcome: StatementOutcome,
    /// True when a read hit the [`MAX_QUERY_ROWS`] fetch cap and its result
    /// holds only the first N rows (FRE-33) — the editor shows a "showing
    /// first N" indicator. Always false for writes.
    pub truncated: bool,
}

/// What a failed script's rollback actually undid — the state the editor
/// reports so the user knows what is in the database (FRE-146).
///
/// Three states rather than a bool because there are three, and the one a
/// bool used to hide is the one that misleads: a rollback that covered the
/// data but not the schema was reported as if it had covered everything.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rollback {
    /// Nothing was undone. The sequential autocommit path, or a failure
    /// before the transaction opened — statements before the failure stand.
    None,
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
/// capability) — then they run sequentially in autocommit, as before.
///
/// Every statement is checked against `caps` *before* any of them runs
/// (FRE-87), so a script that the UI should have gated fails with a clear
/// [`DbError::Unsupported`] and an untouched database, rather than part-way
/// through. The UI disables the run affordance for these cases; this is the
/// backstop for reaching the path anyway.
///
/// `caps` is passed in rather than read from `pool` because the connection's
/// *effective* capabilities are the backend's narrowed by the user's write
/// protection (FRE-111) — see
/// [`Connection::capabilities`](super::registry::Connection::capabilities).
/// Taking it as an argument means a caller cannot accidentally consult the
/// engine's own answer and skip the marking.
pub async fn run_script(
    pool: &DbPool,
    caps: Capabilities,
    statements: &[String],
    on_result: impl FnMut(StatementResult),
) -> Result<(), ScriptError> {
    if let Some(refusal) = refuse_script(caps, statements, pool.dialect()) {
        return Err(refusal);
    }
    if wrap_atomically(caps, pool.dialect(), statements) {
        run_script_atomic(pool, caps, statements, on_result).await
    } else {
        run_script_sequential(pool, statements, on_result).await
    }
}

/// What a rollback covered, given the statements that actually *ran* (FRE-146).
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

/// The autocommit path: each statement commits on its own, so a failure leaves
/// earlier statements' effects in place ([`Rollback::None`]).
async fn run_script_sequential(
    pool: &DbPool,
    statements: &[String],
    mut on_result: impl FnMut(StatementResult),
) -> Result<(), ScriptError> {
    for (statement_index, statement) in statements.iter().enumerate() {
        // Reads stream through the row cap so a huge result never buffers in
        // full; writes report an affected-row count as before.
        let outcome = if classify_statement(statement).returns_rows() {
            pool.query_capped(statement, &[], MAX_QUERY_ROWS)
                .await
                .map(|(result, truncated)| (StatementOutcome::Rows(result), truncated))
        } else {
            pool.execute(statement)
                .await
                .map(|affected| (StatementOutcome::Affected(affected), false))
        };
        match outcome {
            Ok((outcome, truncated)) => on_result(StatementResult {
                preview: statement_preview(statement),
                outcome,
                truncated,
            }),
            Err(error) => {
                return Err(ScriptError {
                    statement_index,
                    preview: statement_preview(statement),
                    error,
                    rollback: Rollback::None,
                })
            }
        }
    }
    Ok(())
}

/// The atomic path: all statements run in one transaction, committed only if
/// every statement succeeds. Any failure (including a failed commit) rolls the
/// script back, and reports what that rollback covered via [`rollback_cover`]
/// — [`Rollback::Full`] unless the engine lets schema changes escape it
/// (FRE-146). Resolved per failure rather than once up front, since it depends
/// on which statements actually ran.
async fn run_script_atomic(
    pool: &DbPool,
    caps: Capabilities,
    statements: &[String],
    mut on_result: impl FnMut(StatementResult),
) -> Result<(), ScriptError> {
    let mut tx = match pool.begin_script_tx().await {
        Ok(tx) => tx,
        // Opening the transaction failed: nothing ran, so nothing rolled back.
        Err(error) => {
            return Err(ScriptError {
                statement_index: 0,
                preview: statements
                    .first()
                    .map(|s| statement_preview(s))
                    .unwrap_or_default(),
                error,
                rollback: Rollback::None,
            })
        }
    };
    for (statement_index, statement) in statements.iter().enumerate() {
        let outcome = if classify_statement(statement).returns_rows() {
            tx.query_capped(statement, MAX_QUERY_ROWS)
                .await
                .map(|(result, truncated)| (StatementOutcome::Rows(result), truncated))
        } else {
            tx.execute(statement)
                .await
                .map(|affected| (StatementOutcome::Affected(affected), false))
        };
        match outcome {
            Ok((outcome, truncated)) => on_result(StatementResult {
                preview: statement_preview(statement),
                outcome,
                truncated,
            }),
            Err(error) => {
                tx.rollback().await;
                return Err(ScriptError {
                    statement_index,
                    preview: statement_preview(statement),
                    error,
                    // Only what ran can have escaped the rollback — a schema
                    // change later in the script never reached the server.
                    rollback: rollback_cover(caps, pool.dialect(), &statements[..=statement_index]),
                });
            }
        }
    }
    if let Err(error) = tx.commit().await {
        // The commit itself failed (e.g. a deferred-constraint violation), not
        // any one statement — label it as such rather than blaming the last
        // statement, which actually succeeded.
        return Err(ScriptError {
            statement_index: statements.len().saturating_sub(1),
            preview: "COMMIT".to_string(),
            error,
            // Every statement ran before the commit was attempted, so the
            // whole script is in scope here.
            rollback: rollback_cover(caps, pool.dialect(), statements),
        });
    }
    Ok(())
}

/// Whether a script should run atomically in one transaction. Only when the
/// connection has the `transactions` capability at all, only for
/// multi-statement scripts, and only when none of the statements manages its
/// own transaction ([`manages_own_transaction`]) or must run outside one
/// ([`is_non_transactional`]) — wrapping either would error at `BEGIN` or on
/// the offending statement. A non-transactional backend falls back to the
/// sequential autocommit path rather than failing.
pub fn wrap_atomically(caps: Capabilities, dialect: Dialect, statements: &[String]) -> bool {
    caps.transactions
        && statements.len() > 1
        && !statements.iter().any(|s| {
            // Stripped once per statement: both checks only need the
            // stripped form (FRE-136).
            let stripped = strip_strings_and_comments(s, dialect);
            manages_own_transaction(&stripped) || is_non_transactional(dialect, &stripped)
        })
}

/// Whether an already-stripped statement issues its own transaction
/// control, so the script is managing atomicity itself and must not be
/// wrapped again.
fn manages_own_transaction(stripped: &str) -> bool {
    matches!(
        leading_words(stripped, 1).first().map(String::as_str),
        Some("begin" | "start" | "commit" | "rollback" | "savepoint" | "release" | "end")
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
        // over-declines call-form read PRAGMAs (`PRAGMA table_info(t)`), but
        // running those sequentially is harmless. Mirrors `needs_confirmation`.
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

    fn split_mssql(sql: &str) -> Vec<String> {
        split_statements(sql, Dialect::SqlServer)
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
            ["SELECT 1", "SELECT 2"]
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
            ["SELECT [a;b] FROM t", "SELECT 2"]
        );
        // `--` and `/*` inside brackets start no comment: the `;` after the
        // identifier still splits instead of being swallowed to end of line.
        assert_eq!(
            split_mssql("SELECT [a--b]; SELECT 2"),
            ["SELECT [a--b]", "SELECT 2"]
        );
        assert_eq!(
            split_mssql("SELECT [a/*b]; SELECT 2"),
            ["SELECT [a/*b]", "SELECT 2"]
        );
        // Quotes inside brackets are plain characters.
        assert_eq!(
            split_mssql("SELECT [a'b]; SELECT 2"),
            ["SELECT [a'b]", "SELECT 2"]
        );
    }

    #[test]
    fn doubled_closing_brackets_stay_inside_the_identifier() {
        // `]]` is an escaped `]`: the identifier is `a]b`, and the `;` after
        // the real closing bracket still splits.
        assert_eq!(
            split_mssql("SELECT [a]]b]; SELECT 2"),
            ["SELECT [a]]b]", "SELECT 2"]
        );
        assert_eq!(
            split_mssql("SELECT [a]];b]; SELECT 2"),
            ["SELECT [a]];b]", "SELECT 2"]
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
            ["SELECT '['", "SELECT 2"]
        );
        assert_eq!(
            split_mssql("SELECT \"[\"; SELECT 2"),
            ["SELECT \"[\"", "SELECT 2"]
        );
        // Likewise inside line and block comments.
        assert_eq!(
            split_mssql("SELECT 1 -- [\n; SELECT 2"),
            ["SELECT 1 -- [", "SELECT 2"]
        );
        assert_eq!(
            split_mssql("SELECT 1 /* [ */; SELECT 2"),
            ["SELECT 1 /* [ */", "SELECT 2"]
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
    fn brackets_stay_inert_on_other_dialects() {
        // Bracket lexing is deliberately SQL Server-only: SQLite accepts
        // [brackets] as a compat quirk but this splitter never lexed them
        // there, and that behavior is unchanged — a `;` inside still splits.
        for dialect in [Dialect::Sqlite, Dialect::Postgres] {
            assert_eq!(
                split_statements("SELECT [a;b] FROM t", dialect),
                ["SELECT [a", "b] FROM t"],
                "{dialect:?}"
            );
        }
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
            assert_eq!(classify_statement(sql), StatementKind::Read, "{sql:?}");
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
            assert_eq!(classify_statement(sql), StatementKind::Ddl, "{sql:?}");
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
            assert_eq!(classify_statement(sql), StatementKind::Write, "{sql:?}");
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
            for sql in [
                "BEGIN",
                "COMMIT",
                "ROLLBACK",
                "SAVEPOINT s",
                "BEGIN TRANSACTION",
            ] {
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

    #[test]
    fn an_unattributable_change_needs_both_capabilities() {
        // SELECT … INTO creates a table and names neither keyword set; a
        // PRAGMA changes state without naming either. Both are charged to
        // both capabilities rather than guessed at.
        //
        // Call-form pragmas (`PRAGMA table_info(t)`) are read-only in fact
        // but land here too, inheriting `needs_confirmation`'s deliberate
        // over-prompt: some pragmas set through the call form, and refusing
        // a read is the safe way to be wrong. Only SQLite and its dialect
        // relatives have pragmas at all, and none of them is a read-only
        // connection today.
        for sql in [
            "SELECT * INTO backup FROM t",
            "PRAGMA journal_mode = WAL",
            "PRAGMA table_info(t)",
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
        // was the fail-unsafe bug this covers (FRE-61).
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
    fn brackets_stay_inert_for_confirmation_on_other_dialects() {
        // Pre-existing behavior preserved: without bracket lexing, `[into]`
        // still tokenizes as the word `into` and over-prompts (fail-safe).
        for dialect in [Dialect::Sqlite, Dialect::Postgres] {
            assert!(
                needs_confirmation("SELECT [into] FROM t", dialect),
                "{dialect:?}"
            );
        }
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
        assert!(stripped.contains("$1")); // parameter placeholder survives
                                          // Length in bytes is preserved and multibyte text stays valid UTF-8.
        let input = "SELECT übercol, 'смузи' FROM t";
        let stripped = strip_strings_and_comments(input, Dialect::Sqlite);
        assert_eq!(stripped.len(), input.len());
        assert!(stripped.contains("übercol"));
        assert!(!stripped.contains("смузи"));
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
            assert!(
                !wraps(Dialect::SqlServer, &stmts(&script)),
                "{script:?} can't run in a transaction on SQL Server"
            );
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
    fn preview_truncates_multibyte_text_on_char_boundaries() {
        let long = format!("SELECT '{}'", "ы".repeat(100));
        let preview = statement_preview(&long);
        assert_eq!(preview.chars().count(), 61);
        assert!(preview.ends_with('…'));
    }
}
