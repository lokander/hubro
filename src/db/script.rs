//! Multi-statement SQL scripts: splitting, classification, and sequential
//! execution.
//!
//! [`split_statements`] is a lexer-level splitter — it understands quotes,
//! comments, and dollar-quoting well enough to find statement boundaries,
//! without parsing SQL. [`run_script`] executes the statements one by one,
//! stopping at the first error; there is no transaction wrapping, so
//! earlier statements' effects persist (v1 semantics — explicit
//! BEGIN/COMMIT in the script works as usual).

use super::error::DbError;
use super::registry::DbPool;
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
/// Statements that are empty (only whitespace and/or comments) are skipped.
/// Known limitation: Postgres `E'…'` escape-string backslash quoting is not
/// understood (`E'\''` misparses); standard SQL doubling (`''`) is fine.
pub fn split_statements(sql: &str) -> Vec<String> {
    let bytes = sql.as_bytes();
    let mut statements = Vec::new();
    let mut start = 0usize;
    // True once the current statement has any non-comment, non-whitespace
    // content — comment-only statements are skipped.
    let mut significant = false;
    let mut i = 0usize;
    while i < bytes.len() {
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
                if !other.is_ascii_whitespace() {
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

/// Whether a statement reads rows or mutates state (decides fetch vs
/// execute, and whether a script needs write confirmation).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatementKind {
    Read,
    Write,
}

/// Classifies by the first significant keyword. The read set is
/// deliberately small; anything unrecognized counts as a write so the
/// confirmation errs on the safe side. `WITH` is treated as a read even
/// though Postgres allows data-modifying CTEs — the rows such a statement
/// returns are still worth showing, and `fetch` executes it all the same.
pub fn classify_statement(sql: &str) -> StatementKind {
    match first_keyword(sql).to_ascii_lowercase().as_str() {
        "select" | "with" | "values" | "show" | "explain" | "pragma" | "table" => {
            StatementKind::Read
        }
        _ => StatementKind::Write,
    }
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
}

/// Where and how a script failed. Statements before `statement_index`
/// completed and their effects persist (no transaction wrapping).
#[derive(Debug, Clone, PartialEq)]
pub struct ScriptError {
    /// Index into the script's statement list.
    pub statement_index: usize,
    /// Preview of the failing statement.
    pub preview: String,
    pub error: DbError,
}

/// Runs a script's statements sequentially, calling `on_result` after each
/// successful statement (so callers can show progress), and stopping at the
/// first failure.
pub async fn run_script(
    pool: &DbPool,
    statements: &[String],
    mut on_result: impl FnMut(StatementResult),
) -> Result<(), ScriptError> {
    for (statement_index, statement) in statements.iter().enumerate() {
        let outcome = match classify_statement(statement) {
            StatementKind::Read => pool.query(statement).await.map(StatementOutcome::Rows),
            StatementKind::Write => pool
                .execute(statement)
                .await
                .map(StatementOutcome::Affected),
        };
        match outcome {
            Ok(outcome) => on_result(StatementResult {
                preview: statement_preview(statement),
                outcome,
            }),
            Err(error) => {
                return Err(ScriptError {
                    statement_index,
                    preview: statement_preview(statement),
                    error,
                })
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_on_semicolons_and_keeps_a_trailing_statement() {
        assert_eq!(
            split_statements("SELECT 1; SELECT 2 ; SELECT 3"),
            ["SELECT 1", "SELECT 2", "SELECT 3"]
        );
    }

    #[test]
    fn a_single_statement_needs_no_semicolon() {
        assert_eq!(split_statements("SELECT 1"), ["SELECT 1"]);
        assert_eq!(split_statements("  SELECT 1;  "), ["SELECT 1"]);
    }

    #[test]
    fn semicolons_inside_quotes_do_not_split() {
        assert_eq!(
            split_statements("SELECT 'a;b'; SELECT \"c;d\""),
            ["SELECT 'a;b'", "SELECT \"c;d\""]
        );
        // Doubled quotes stay inside the string.
        assert_eq!(
            split_statements("SELECT 'it''s;fine'; SELECT 2"),
            ["SELECT 'it''s;fine'", "SELECT 2"]
        );
    }

    #[test]
    fn semicolons_inside_comments_do_not_split() {
        assert_eq!(
            split_statements("SELECT 1 -- trailing; comment\n; SELECT 2"),
            ["SELECT 1 -- trailing; comment", "SELECT 2"]
        );
        assert_eq!(
            split_statements("SELECT /* a;b */ 1; SELECT 2"),
            ["SELECT /* a;b */ 1", "SELECT 2"]
        );
        // Nested block comments (Postgres nests; harmless for SQLite).
        assert_eq!(
            split_statements("SELECT /* x /* y; */ z; */ 1; SELECT 2"),
            ["SELECT /* x /* y; */ z; */ 1", "SELECT 2"]
        );
    }

    #[test]
    fn dollar_quoted_bodies_do_not_split() {
        assert_eq!(
            split_statements("SELECT $$a;b$$; SELECT 2"),
            ["SELECT $$a;b$$", "SELECT 2"]
        );
        assert_eq!(
            split_statements("CREATE FUNCTION f() AS $fn$ BEGIN; END; $fn$; SELECT 2"),
            ["CREATE FUNCTION f() AS $fn$ BEGIN; END; $fn$", "SELECT 2"]
        );
        // A different tag inside the body does not close the quote.
        assert_eq!(
            split_statements("SELECT $a$ x $b$ ; $a$; SELECT 2"),
            ["SELECT $a$ x $b$ ; $a$", "SELECT 2"]
        );
        // $1 is a parameter, not a delimiter.
        assert_eq!(
            split_statements("SELECT $1; SELECT $2"),
            ["SELECT $1", "SELECT $2"]
        );
    }

    #[test]
    fn unterminated_quotes_swallow_the_rest() {
        assert_eq!(split_statements("SELECT 'a; b"), ["SELECT 'a; b"]);
        assert_eq!(split_statements("SELECT $$a; b"), ["SELECT $$a; b"]);
    }

    #[test]
    fn empty_and_comment_only_statements_are_skipped() {
        assert_eq!(split_statements(""), Vec::<String>::new());
        assert_eq!(split_statements(" ;  ; ;"), Vec::<String>::new());
        assert_eq!(
            split_statements("-- just a comment\n; /* and another */;"),
            Vec::<String>::new()
        );
        assert_eq!(split_statements(";;SELECT 1;; -- done\n;"), ["SELECT 1"]);
    }

    #[test]
    fn multibyte_content_splits_cleanly() {
        assert_eq!(
            split_statements("SELECT 'смузи;ярлык'; SELECT 'ünïcödé'"),
            ["SELECT 'смузи;ярлык'", "SELECT 'ünïcödé'"]
        );
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

    #[test]
    fn preview_truncates_multibyte_text_on_char_boundaries() {
        let long = format!("SELECT '{}'", "ы".repeat(100));
        let preview = statement_preview(&long);
        assert_eq!(preview.chars().count(), 61);
        assert!(preview.ends_with('…'));
    }
}
