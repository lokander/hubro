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

use super::error::DbError;
use super::page::Dialect;
use super::registry::{DbPool, MAX_QUERY_ROWS};
use super::value::QueryResult;

/// Splits a script into individual statements on `;`, respecting:
///
/// - single- and double-quoted strings (`'…'`, `"…"`; doubled quotes stay
///   inside naturally, since each quote char toggles the string state)
/// - dollar-quoted strings (`$$…$$`, `$tag$…$tag$` — Postgres)
/// - line comments (`-- …`) and block comments (`/* … */`, nesting like
///   Postgres; SQLite never nests but treating `/*` inside a comment as
///   nested is harmless there)
/// - a trailing statement without a semicolon
///
/// On SQL Server, `GO` also separates statements. `GO` is a client-side
/// batch separator (SSMS/sqlcmd), not T-SQL: it only counts when it stands
/// alone on its own line (leading whitespace allowed), optionally followed
/// by a repeat count — `GO 5` is treated as a plain separator with the
/// count ignored (repeating a batch is a scripting-tool feature, not
/// something a viewer should replay). `GO` inside strings or comments, or
/// sharing a line with other code, never splits.
///
/// Statements that are empty (only whitespace and/or comments) are skipped.
/// Known limitation: Postgres `E'…'` escape-string backslash quoting is not
/// understood (`E'\''` misparses); standard SQL doubling (`''`) is fine.
pub fn split_statements(sql: &str, dialect: Dialect) -> Vec<String> {
    let bytes = sql.as_bytes();
    let mut statements = Vec::new();
    let mut start = 0usize;
    // True once the current statement has any non-comment, non-whitespace
    // content — comment-only statements are skipped.
    let mut significant = false;
    // Where the current line starts, for GO detection: a separator line has
    // nothing but whitespace before the `GO`. Newlines consumed inside
    // strings/comments deliberately do not advance this — the stale prefix
    // then contains non-whitespace, so a `GO` on such a line never splits.
    let mut line_start = 0usize;
    let mut i = 0usize;
    while i < bytes.len() {
        if dialect == Dialect::SqlServer
            && matches!(bytes[i], b'g' | b'G')
            && bytes[line_start..i].iter().all(u8::is_ascii_whitespace)
        {
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
            quote @ (b'\'' | b'"') => {
                significant = true;
                i += 1;
                while i < bytes.len() && bytes[i] != quote {
                    i += 1;
                }
                i += 1; // past the closing quote (or end of input)
            }
            b'-' if bytes.get(i + 1) == Some(&b'-') => {
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if bytes.get(i + 1) == Some(&b'*') => {
                let mut depth = 1usize;
                i += 2;
                while i < bytes.len() && depth > 0 {
                    if bytes[i] == b'/' && bytes.get(i + 1) == Some(&b'*') {
                        depth += 1;
                        i += 2;
                    } else if bytes[i] == b'*' && bytes.get(i + 1) == Some(&b'/') {
                        depth -= 1;
                        i += 2;
                    } else {
                        i += 1;
                    }
                }
            }
            b'$' => {
                significant = true;
                if let Some(open_end) = dollar_tag_end(bytes, i) {
                    let delimiter = &sql[i..open_end];
                    match sql[open_end..].find(delimiter) {
                        Some(close) => i = open_end + close + delimiter.len(),
                        None => i = bytes.len(), // unterminated: rest is the string
                    }
                } else {
                    i += 1; // a bare '$' (e.g. a $1 parameter placeholder)
                }
            }
            b';' => {
                if significant {
                    statements.push(sql[start..i].trim().to_string());
                }
                start = i + 1;
                significant = false;
                i += 1;
            }
            other => {
                if other == b'\n' {
                    line_start = i + 1;
                } else if !other.is_ascii_whitespace() {
                    significant = true;
                }
                i += 1;
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

/// Whether a statement returns rows (executed via fetch) or not (executed
/// via execute, reporting an affected-row count). This is an execution-mode
/// choice only — see [`needs_confirmation`] for the "can this mutate?"
/// question, which is deliberately stricter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatementKind {
    Read,
    Write,
}

/// Classifies by the first significant keyword. The read set is
/// deliberately small; anything unrecognized counts as a write. `WITH` is
/// classified as a read even though Postgres allows data-modifying CTEs —
/// the rows such a statement returns are still worth showing, and `fetch`
/// executes it all the same (the confirmation banner is handled separately
/// by [`needs_confirmation`]).
pub fn classify_statement(sql: &str) -> StatementKind {
    match first_keyword(sql).to_ascii_lowercase().as_str() {
        "select" | "with" | "values" | "show" | "explain" | "pragma" | "table" => {
            StatementKind::Read
        }
        _ => StatementKind::Write,
    }
}

/// Keywords that mark a fetch-classified statement as potentially mutating
/// when they appear anywhere outside strings and comments.
const EMBEDDED_WRITE_KEYWORDS: [&str; 9] = [
    "insert", "update", "delete", "merge", "create", "drop", "alter", "truncate", "replace",
];

/// Whether running this statement can mutate the database, i.e. whether the
/// editor must ask before running it. Everything [`classify_statement`]
/// calls a write needs confirmation; on top of that, fetch-classified
/// statements are token-scanned (outside strings/comments, so quoted
/// literals never trigger) for embedded write forms:
///
/// - `WITH` / `EXPLAIN`: any [`EMBEDDED_WRITE_KEYWORDS`] token anywhere —
///   catches data-modifying CTEs (`WITH x AS (DELETE …) SELECT …`) and
///   `EXPLAIN ANALYZE UPDATE …`, which Postgres actually executes. Plain
///   `EXPLAIN SELECT` / `EXPLAIN ANALYZE SELECT` do not prompt.
/// - `SELECT`: an `INTO` token — `SELECT … INTO new_table` creates a table.
/// - `PRAGMA`: a `=` or `(` — the value-setting forms. This deliberately
///   over-prompts call-form read pragmas like `PRAGMA table_info(t)`: some
///   pragmas accept both spellings for setting, and prompting is the
///   fail-safe side.
pub fn needs_confirmation(sql: &str) -> bool {
    if classify_statement(sql) == StatementKind::Write {
        return true;
    }
    match first_keyword(sql).to_ascii_lowercase().as_str() {
        "with" | "explain" => has_top_level_word(sql, |word| {
            EMBEDDED_WRITE_KEYWORDS.contains(&word.to_ascii_lowercase().as_str())
        }),
        "select" => has_top_level_word(sql, |word| word.eq_ignore_ascii_case("into")),
        "pragma" => {
            let code = strip_strings_and_comments(sql);
            code.contains('=') || code.contains('(')
        }
        _ => false,
    }
}

/// Whether any word-ish token (identifier characters) of the statement —
/// with strings and comments removed — matches the predicate.
fn has_top_level_word(sql: &str, matches: impl Fn(&str) -> bool) -> bool {
    strip_strings_and_comments(sql)
        .split(|c: char| !(c.is_alphanumeric() || c == '_'))
        .any(|word| !word.is_empty() && matches(word))
}

/// The statement text with quoted strings (single, double, dollar) and
/// comments blanked out to spaces, so token scans can't be fooled by
/// literals like `'please do not DELETE me'`. Same lexer states as
/// [`split_statements`].
fn strip_strings_and_comments(sql: &str) -> String {
    let bytes = sql.as_bytes();
    let mut out = vec![b' '; bytes.len()];
    let mut i = 0usize;
    while i < bytes.len() {
        match bytes[i] {
            quote @ (b'\'' | b'"') => {
                i += 1;
                while i < bytes.len() && bytes[i] != quote {
                    i += 1;
                }
                i += 1;
            }
            b'-' if bytes.get(i + 1) == Some(&b'-') => {
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if bytes.get(i + 1) == Some(&b'*') => {
                let mut depth = 1usize;
                i += 2;
                while i < bytes.len() && depth > 0 {
                    if bytes[i] == b'/' && bytes.get(i + 1) == Some(&b'*') {
                        depth += 1;
                        i += 2;
                    } else if bytes[i] == b'*' && bytes.get(i + 1) == Some(&b'/') {
                        depth -= 1;
                        i += 2;
                    } else {
                        i += 1;
                    }
                }
            }
            b'$' => {
                if let Some(open_end) = dollar_tag_end(bytes, i) {
                    let delimiter = &sql[i..open_end];
                    match sql[open_end..].find(delimiter) {
                        Some(close) => i = open_end + close + delimiter.len(),
                        None => i = bytes.len(),
                    }
                } else {
                    out[i] = b'$';
                    i += 1;
                }
            }
            c => {
                out[i] = c;
                i += 1;
            }
        }
    }
    // Multibyte chars survive intact: the fallthrough arm copies them byte
    // by byte across iterations, and blanked regions always start and end
    // at ASCII delimiters (a UTF-8 continuation byte can never equal an
    // ASCII quote/comment marker), so sequences are never split.
    String::from_utf8_lossy(&out).into_owned()
}

/// The first keyword of a statement, skipping leading whitespace, comments,
/// and opening parentheses (`(SELECT …)` is a read).
fn first_keyword(sql: &str) -> &str {
    let bytes = sql.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        match bytes[i] {
            c if c.is_ascii_whitespace() || c == b'(' => i += 1,
            b'-' if bytes.get(i + 1) == Some(&b'-') => {
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if bytes.get(i + 1) == Some(&b'*') => {
                let mut depth = 1usize;
                i += 2;
                while i < bytes.len() && depth > 0 {
                    if bytes[i] == b'/' && bytes.get(i + 1) == Some(&b'*') {
                        depth += 1;
                        i += 2;
                    } else if bytes[i] == b'*' && bytes.get(i + 1) == Some(&b'/') {
                        depth -= 1;
                        i += 2;
                    } else {
                        i += 1;
                    }
                }
            }
            _ => break,
        }
    }
    let start = i;
    while i < bytes.len() && (bytes[i].is_ascii_alphabetic() || bytes[i] == b'_') {
        i += 1;
    }
    &sql[start..i]
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

/// Where and how a script failed.
#[derive(Debug, Clone, PartialEq)]
pub struct ScriptError {
    /// Index into the script's statement list.
    pub statement_index: usize,
    /// Preview of the failing statement.
    pub preview: String,
    pub error: DbError,
    /// Whether the whole script was rolled back (atomic run) rather than
    /// leaving the statements before `statement_index` committed (sequential
    /// run). The editor surfaces this so the user knows the database state.
    pub rolled_back: bool,
}

/// Runs a script's statements, calling `on_result` after each successful
/// statement (so callers can show progress) and stopping at the first failure.
///
/// Multi-statement scripts run atomically in one transaction unless
/// [`wrap_atomically`] declines (self-managed transactions or a
/// non-transactional statement) — then they run sequentially in autocommit,
/// as before.
pub async fn run_script(
    pool: &DbPool,
    statements: &[String],
    on_result: impl FnMut(StatementResult),
) -> Result<(), ScriptError> {
    if wrap_atomically(pool.dialect(), statements) {
        run_script_atomic(pool, statements, on_result).await
    } else {
        run_script_sequential(pool, statements, on_result).await
    }
}

/// The autocommit path: each statement commits on its own, so a failure leaves
/// earlier statements' effects in place (`rolled_back: false`).
async fn run_script_sequential(
    pool: &DbPool,
    statements: &[String],
    mut on_result: impl FnMut(StatementResult),
) -> Result<(), ScriptError> {
    for (statement_index, statement) in statements.iter().enumerate() {
        // Reads stream through the row cap so a huge result never buffers in
        // full; writes report an affected-row count as before.
        let outcome = match classify_statement(statement) {
            StatementKind::Read => pool
                .query_capped(statement, &[], MAX_QUERY_ROWS)
                .await
                .map(|(result, truncated)| (StatementOutcome::Rows(result), truncated)),
            StatementKind::Write => pool
                .execute(statement)
                .await
                .map(|affected| (StatementOutcome::Affected(affected), false)),
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
                    rolled_back: false,
                })
            }
        }
    }
    Ok(())
}

/// The atomic path: all statements run in one transaction, committed only if
/// every statement succeeds. Any failure (including a failed commit) rolls the
/// whole script back (`rolled_back: true`).
async fn run_script_atomic(
    pool: &DbPool,
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
                rolled_back: false,
            })
        }
    };
    for (statement_index, statement) in statements.iter().enumerate() {
        let outcome = match classify_statement(statement) {
            StatementKind::Read => tx
                .query_capped(statement, MAX_QUERY_ROWS)
                .await
                .map(|(result, truncated)| (StatementOutcome::Rows(result), truncated)),
            StatementKind::Write => tx
                .execute(statement)
                .await
                .map(|affected| (StatementOutcome::Affected(affected), false)),
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
                    rolled_back: true,
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
            rolled_back: true,
        });
    }
    Ok(())
}

/// Whether a script should run atomically in one transaction. Only for
/// multi-statement scripts, and only when none of the statements manages its
/// own transaction ([`manages_own_transaction`]) or must run outside one
/// ([`is_non_transactional`]) — wrapping either would error at `BEGIN` or on
/// the offending statement.
pub fn wrap_atomically(dialect: Dialect, statements: &[String]) -> bool {
    statements.len() > 1
        && !statements
            .iter()
            .any(|s| manages_own_transaction(s) || is_non_transactional(dialect, s))
}

/// Whether a statement issues its own transaction control, so the script is
/// managing atomicity itself and must not be wrapped again.
fn manages_own_transaction(sql: &str) -> bool {
    matches!(
        leading_words(sql, 1).first().map(String::as_str),
        Some("begin" | "start" | "commit" | "rollback" | "savepoint" | "release" | "end")
    )
}

/// Whether a statement can't run (or won't take effect) inside a transaction
/// block, so the script must run sequentially instead of wrapped. `VACUUM`
/// applies to SQLite and Postgres alike (and doesn't exist elsewhere, so the
/// unconditional check is harmless); SQLite value-setting `PRAGMA`s are
/// *silently ignored* in a transaction (worse than erroring — they'd vanish
/// without a trace); the Postgres set errors with "cannot run inside a
/// transaction block"; the SQL Server set covers server-level operations
/// T-SQL refuses inside a user transaction (`CREATE`/`ALTER`/`DROP
/// DATABASE`, `BACKUP`, `RESTORE`, full-text DDL).
fn is_non_transactional(dialect: Dialect, sql: &str) -> bool {
    let words = leading_words(sql, 2);
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
        let code = strip_strings_and_comments(sql);
        if code.contains('=') || code.contains('(') {
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
        if has_top_level_word(sql, |word| word.eq_ignore_ascii_case("concurrently")) {
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

/// The first `n` word tokens of a statement (lowercased), with strings and
/// comments removed so a leading comment or quoted text can't masquerade as a
/// keyword.
fn leading_words(sql: &str, n: usize) -> Vec<String> {
    strip_strings_and_comments(sql)
        .split(|c: char| !(c.is_alphanumeric() || c == '_'))
        .filter(|word| !word.is_empty())
        .take(n)
        .map(|word| word.to_ascii_lowercase())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!(
            split("SELECT $1; SELECT $2"),
            ["SELECT $1", "SELECT $2"]
        );
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
        assert_eq!(split_mssql("SELECT 1;\nGO\nSELECT 2"), ["SELECT 1", "SELECT 2"]);
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
        assert_eq!(
            split_mssql("SELECT 'a\nGO\nb'"),
            ["SELECT 'a\nGO\nb'"]
        );
        assert_eq!(split_mssql("SELECT 'GO'"), ["SELECT 'GO'"]);
        // Inside comments.
        assert_eq!(
            split_mssql("SELECT 1 -- GO\n+ 2"),
            ["SELECT 1 -- GO\n+ 2"]
        );
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
    fn everything_else_is_a_write() {
        for sql in [
            "INSERT INTO t VALUES (1)",
            "UPDATE t SET a = 1",
            "DELETE FROM t",
            "CREATE TABLE t (a int)",
            "DROP TABLE t",
            "ALTER TABLE t ADD b int",
            "TRUNCATE t",
            "BEGIN",
            "VACUUM",
            "GRANT ALL ON t TO x",
            "COPY t FROM stdin",
            "", // unclassifiable: err on the safe side
        ] {
            assert_eq!(classify_statement(sql), StatementKind::Write, "{sql:?}");
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
            assert!(needs_confirmation(sql), "{sql:?} must need confirmation");
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
            assert!(!needs_confirmation(sql), "{sql:?} must not prompt");
        }
    }

    #[test]
    fn classified_writes_always_need_confirmation() {
        for sql in ["INSERT INTO t VALUES (1)", "DROP TABLE t", "BEGIN", ""] {
            assert!(needs_confirmation(sql), "{sql:?} must need confirmation");
        }
    }

    #[test]
    fn strip_strings_and_comments_blanks_only_literals_and_comments() {
        let stripped = strip_strings_and_comments("SELECT 'a;b', \"q\" -- c\nFROM t /* x */");
        assert!(stripped.contains("SELECT"));
        assert!(stripped.contains("FROM t"));
        for gone in ["a;b", "q", "c", "x", "'", "\"", "--", "/*"] {
            assert!(!stripped.contains(gone), "{gone:?} should be blanked");
        }
        let stripped = strip_strings_and_comments("SELECT $$drop$$, $t$delete$t$, $1");
        assert!(!stripped.contains("drop"));
        assert!(!stripped.contains("delete"));
        assert!(stripped.contains("$1")); // parameter placeholder survives
                                          // Length in bytes is preserved and multibyte text stays valid UTF-8.
        let input = "SELECT übercol, 'смузи' FROM t";
        let stripped = strip_strings_and_comments(input);
        assert_eq!(stripped.len(), input.len());
        assert!(stripped.contains("übercol"));
        assert!(!stripped.contains("смузи"));
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
    fn multi_statement_scripts_wrap_atomically_by_default() {
        for dialect in [Dialect::Sqlite, Dialect::Postgres, Dialect::SqlServer] {
            assert!(wrap_atomically(
                dialect,
                &stmts(&["INSERT INTO t VALUES (1)", "UPDATE t SET a = 2"])
            ));
            // Mixed read/write still wraps.
            assert!(wrap_atomically(
                dialect,
                &stmts(&["DELETE FROM t WHERE a = 1", "SELECT count(*) FROM t"])
            ));
        }
    }

    #[test]
    fn single_statement_scripts_do_not_wrap() {
        // One statement is already atomic under autocommit — nothing to wrap.
        for dialect in [Dialect::Sqlite, Dialect::Postgres, Dialect::SqlServer] {
            assert!(!wrap_atomically(dialect, &stmts(&["DELETE FROM t"])));
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
                    !wrap_atomically(dialect, &stmts(&script)),
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
            assert!(!wrap_atomically(
                dialect,
                &stmts(&["DELETE FROM t", "VACUUM"])
            ));
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
                !wrap_atomically(Dialect::Postgres, &stmts(&script)),
                "{script:?} can't run in a transaction on Postgres"
            );
        }
        // `CONCURRENTLY` inside a string/comment must not trip the check.
        assert!(wrap_atomically(
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
            vec!["CREATE TABLE t (a int)", "CREATE FULLTEXT INDEX ON t (a) KEY INDEX pk"],
        ] {
            assert!(
                !wrap_atomically(Dialect::SqlServer, &stmts(&script)),
                "{script:?} can't run in a transaction on SQL Server"
            );
        }
        // The same statements don't decline wrapping for the wrong dialect
        // reasons on SQL Server: ordinary DDL/DML still wraps.
        assert!(wrap_atomically(
            Dialect::SqlServer,
            &stmts(&["CREATE TABLE t (a int)", "INSERT INTO t VALUES (1)"])
        ));
        // Keywords inside strings must not trip the check.
        assert!(wrap_atomically(
            Dialect::SqlServer,
            &stmts(&[
                "INSERT INTO t VALUES ('backup this later')",
                "UPDATE t SET a = 1",
            ])
        ));
        // Postgres-only exclusions don't leak into SQL Server scripts
        // (CONCURRENTLY is not a T-SQL concept).
        assert!(wrap_atomically(
            Dialect::SqlServer,
            &stmts(&["SELECT 1", "DROP INDEX CONCURRENTLY i"])
        ));
        // And the SQL Server set doesn't leak into SQLite/Postgres. (CREATE
        // DATABASE is already non-transactional on Postgres; BACKUP is the
        // discriminating case.)
        for dialect in [Dialect::Sqlite, Dialect::Postgres] {
            assert!(wrap_atomically(
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
                !wrap_atomically(Dialect::Sqlite, &stmts(&script)),
                "{script:?} should run sequentially so the PRAGMA applies"
            );
        }
        // A bare read PRAGMA (no value) is transaction-safe, so it still wraps.
        assert!(wrap_atomically(
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
