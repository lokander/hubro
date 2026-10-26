//! The test-support rules that live in `tests/common`, checked once.
//!
//! `tests/common/mod.rs` is compiled into every integration binary that says
//! `mod common;`, so a `#[test]` written there runs once per binary — fifteen
//! and more times per `cargo test`, for checks that read nothing but their own
//! arguments. They live here instead, and
//! [`tests_common_holds_no_tests`] keeps it that way.
//!
//! The rest of the file is the rule `tests/CLAUDE.md` states and nothing used
//! to check: a suite reads its engine's URL through `tests/common` —
//! `common::pg_test_url`, `common::mssql_test_url` or `common::engine_url` —
//! never from the environment itself. Reading `HUBRO_PG_TEST_URL` directly
//! puts a suite back in the database every other suite shares, with their
//! identically named fixtures; reading any engine's variable directly skips
//! [`common::REQUIRE_TEST_URLS`], so a run that was meant to reach the server
//! skips and passes instead.

mod common;

use common::{
    is_required, pg_major_mismatch, with_database, Engine, PG_TEST_MAJOR, REQUIRE_TEST_URLS,
};

// ---------------------------------------------------------------------------
// Reading source with its comments removed
// ---------------------------------------------------------------------------

/// `src` with its comments blanked to spaces, and for each byte of the result
/// whether it sits inside a string or character literal.
///
/// Comments go because a rule stated in a comment is not a call; literals are
/// marked rather than removed because a call's argument *is* one, and a
/// literal that merely mentions a call (as this file's own cases do) must not
/// count as one. Handles `"…"` with escapes, raw strings `r#"…"#`, byte
/// strings, nested block comments, and tells a character literal from a
/// lifetime.
fn code_of(src: &str) -> (String, Vec<bool>) {
    let bytes = src.as_bytes();
    let mut out = bytes.to_vec();
    let mut literal = vec![false; bytes.len()];
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'/' if bytes.get(i + 1) == Some(&b'/') => {
                while i < bytes.len() && bytes[i] != b'\n' {
                    out[i] = b' ';
                    i += 1;
                }
            }
            b'/' if bytes.get(i + 1) == Some(&b'*') => {
                let mut depth = 0;
                loop {
                    if i >= bytes.len() {
                        break;
                    }
                    if bytes[i] == b'/' && bytes.get(i + 1) == Some(&b'*') {
                        depth += 1;
                        out[i] = b' ';
                        out[i + 1] = b' ';
                        i += 2;
                    } else if bytes[i] == b'*' && bytes.get(i + 1) == Some(&b'/') {
                        depth -= 1;
                        out[i] = b' ';
                        out[i + 1] = b' ';
                        i += 2;
                        if depth == 0 {
                            break;
                        }
                    } else {
                        if bytes[i] != b'\n' {
                            out[i] = b' ';
                        }
                        i += 1;
                    }
                }
            }
            b'r' if matches!(bytes.get(i + 1), Some(b'"' | b'#')) && starts_a_token(bytes, i) => {
                let mut j = i + 1;
                while bytes.get(j) == Some(&b'#') {
                    j += 1;
                }
                if bytes.get(j) != Some(&b'"') {
                    i += 1;
                    continue;
                }
                let hashes = j - i - 1;
                let mut end = j + 1;
                while end < bytes.len()
                    && !(bytes[end] == b'"'
                        && bytes[end + 1..]
                            .iter()
                            .take(hashes)
                            .filter(|b| **b == b'#')
                            .count()
                            == hashes)
                {
                    end += 1;
                }
                let end = (end + 1 + hashes).min(bytes.len());
                literal[j..end].iter_mut().for_each(|l| *l = true);
                i = end;
            }
            b'"' => {
                let start = i;
                i += 1;
                while i < bytes.len() && bytes[i] != b'"' {
                    i += if bytes[i] == b'\\' { 2 } else { 1 };
                }
                i = (i + 1).min(bytes.len());
                literal[start..i].iter_mut().for_each(|l| *l = true);
            }
            b'\'' => {
                // `'\x'`-style escapes and `'c'` are literals; `'a` is a lifetime.
                let close = if bytes.get(i + 1) == Some(&b'\\') {
                    bytes[i + 2..]
                        .iter()
                        .position(|b| *b == b'\'')
                        .map(|p| i + 2 + p)
                } else {
                    let width = src[i + 1..].chars().next().map_or(1, char::len_utf8);
                    (bytes.get(i + 1 + width) == Some(&b'\'')).then_some(i + 1 + width)
                };
                match close {
                    Some(close) => {
                        literal[i..=close].iter_mut().for_each(|l| *l = true);
                        i = close + 1;
                    }
                    None => i += 1,
                }
            }
            _ => i += 1,
        }
    }
    (
        String::from_utf8(out).expect("blanking keeps UTF-8"),
        literal,
    )
}

/// Whether the `r` at `at` begins a raw string: not the tail of an
/// identifier, though it may follow the `b` of a byte string.
fn starts_a_token(bytes: &[u8], at: usize) -> bool {
    let before = |n: usize| at.checked_sub(n).map(|i| bytes[i]);
    match before(1) {
        None => true,
        Some(b'b') => !before(2).is_some_and(is_ident),
        Some(b) => !is_ident(b),
    }
}

fn is_ident(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// Every way `src` reads a test URL from the environment itself, one line
/// each — empty when it reads none.
///
/// Reading the environment is allowed; the rule is about *which* variable. A
/// call is fine when its argument is a string literal that names no
/// `…_TEST_URL`. Anything else is reported, because it cannot be told apart
/// from the bypass: a helper taking the variable's name as a parameter (five
/// suites had one), an aliased import (`use std::env::var`), `env::vars()`.
fn raw_url_reads(src: &str) -> Vec<String> {
    let (code, literal) = code_of(src);
    let mut found = Vec::new();
    let line_of = |at: usize| code[..at].lines().count().max(1);
    for (at, _) in code.match_indices("env::") {
        if literal[at] {
            continue;
        }
        let rest = &code[at + "env::".len()..];
        let call = ["var_os(", "var("]
            .into_iter()
            .find(|call| rest.starts_with(call));
        let name_starts = rest.starts_with("var") || rest.starts_with('{');
        match call {
            Some(call) => {
                let open = at + "env::".len() + call.len() - 1;
                let argument = argument_at(&code, &literal, open);
                if !is_plain_name(argument) {
                    found.push(format!(
                        "line {}: reads env::{call}{argument}) — the variable must be a \
                         literal naming no test URL",
                        line_of(at)
                    ));
                }
            }
            None if name_starts => found.push(format!(
                "line {}: names env::{} other than as a direct call",
                line_of(at),
                rest.split(|c: char| !(is_ident(c as u8) || c == '{'))
                    .next()
                    .unwrap_or("")
            )),
            None => {}
        }
    }
    for (at, _) in code.match_indices("env!(") {
        if literal[at] {
            continue;
        }
        let argument = argument_at(&code, &literal, at + "env!".len());
        if !is_plain_name(argument) {
            found.push(format!(
                "line {}: reads env!({argument}) at compile time",
                line_of(at)
            ));
        }
    }
    found
}

/// The text between the `(` at `open` and its matching `)`, trimmed.
fn argument_at<'a>(code: &'a str, literal: &[bool], open: usize) -> &'a str {
    let mut depth = 0;
    for (offset, b) in code.as_bytes()[open..].iter().enumerate() {
        let at = open + offset;
        if literal[at] {
            continue;
        }
        match b {
            b'(' => depth += 1,
            b')' => {
                depth -= 1;
                if depth == 0 {
                    return code[open + 1..at].trim();
                }
            }
            _ => {}
        }
    }
    code[open + 1..].trim()
}

/// Whether `argument` is one string literal naming no test URL.
fn is_plain_name(argument: &str) -> bool {
    let Some(name) = argument
        .strip_prefix('"')
        .and_then(|rest| rest.strip_suffix('"'))
    else {
        return false;
    };
    name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') && !name.contains("_TEST_URL")
}

/// Every `.rs` file under `dir`, recursively.
fn rust_files(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut files = Vec::new();
    for entry in std::fs::read_dir(dir).unwrap_or_else(|e| panic!("{}: {e}", dir.display())) {
        let path = entry.unwrap().path();
        if path.is_dir() {
            files.extend(rust_files(&path));
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            files.push(path);
        }
    }
    files
}

fn tests_dir() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests")
}

#[test]
fn no_suite_reads_a_test_url_from_the_environment_itself() {
    let common = tests_dir().join("common");
    let mut offenders = Vec::new();
    let files = rust_files(&tests_dir());
    for path in &files {
        if path.starts_with(&common) {
            continue;
        }
        let src = std::fs::read_to_string(path).unwrap();
        for read in raw_url_reads(&src) {
            offenders.push(format!("{}: {read}", path.display()));
        }
    }
    // A scan that found nothing to scan would pass too.
    assert!(
        files.len() > 30,
        "only {} test files found under {}",
        files.len(),
        tests_dir().display()
    );
    assert!(
        offenders.is_empty(),
        "read the engine URL through common::pg_test_url, common::mssql_test_url \
         or common::engine_url instead:\n{}",
        offenders.join("\n")
    );
}

#[test]
fn the_scan_tells_a_raw_url_read_from_everything_else() {
    for raw in [
        r#"fn u() { std::env::var("HUBRO_PG_TEST_URL").ok() }"#,
        r#"fn u() { env::var( "HUBRO_CITUS_TEST_URL" ) }"#,
        r#"fn u() { std::env::var_os("HUBRO_MSSQL_TEST_URL") }"#,
        // A helper taking the name is the shape five suites had.
        "fn url(var: &str) -> Option<String> { std::env::var(var).ok() }",
        "use std::env::var;\nfn u() { var(\"HUBRO_PG_TEST_URL\") }",
        "use std::env::{self, var_os};",
        "fn u() { std::env::vars().count() }",
        r#"const U: Option<&str> = option_env!("HUBRO_PG_TEST_URL");"#,
        // A `//` inside a string does not start a comment.
        r#"fn u() { let a = "postgres://x"; std::env::var("HUBRO_PG_TEST_URL") }"#,
        // Nor does a lifetime start a character literal that hides the call.
        r#"fn u<'a>(x: &'a str) { std::env::var("HUBRO_PG_TEST_URL"); }"#,
    ] {
        assert!(!raw_url_reads(raw).is_empty(), "missed: {raw}");
    }
    for clean in [
        r#"fn u() { std::env::var("HUBRO_SSH_TEST_HOST") }"#,
        r#"fn u() { env!("CARGO_MANIFEST_DIR") }"#,
        "// std::env::var(\"HUBRO_PG_TEST_URL\") in a comment",
        "/* std::env::var(var) /* nested */ still a comment */",
        r#"fn u() { let s = "std::env::var(var)"; }"#,
        r##"fn u() { let s = r#"std::env::var("HUBRO_PG_TEST_URL")"#; }"##,
        r#"fn u() { let q = '"'; common::engine_url("HUBRO_CITUS_TEST_URL") }"#,
        r##"fn u() { let s = br#"\" std::env::var(var)"#; }"##,
    ] {
        assert_eq!(raw_url_reads(clean), Vec::<String>::new(), "{clean}");
    }
}

#[test]
fn engine_url_refuses_the_per_process_engines_and_unknown_names() {
    for refused in [
        Engine::Postgres.env(),
        Engine::SqlServer.env(),
        "HUBRO_CITUS_TEST_UR",
    ] {
        assert!(
            std::panic::catch_unwind(|| common::engine_url(refused)).is_err(),
            "engine_url({refused:?}) should refuse"
        );
    }
}

#[test]
fn tests_common_holds_no_tests() {
    for path in rust_files(&tests_dir().join("common")) {
        let src = std::fs::read_to_string(&path).unwrap();
        let (code, literal) = code_of(&src);
        let attributes: String = code
            .char_indices()
            .filter(|(at, c)| !literal[*at] && !c.is_whitespace())
            .map(|(_, c)| c)
            .collect();
        for marker in ["#[test]", "#[tokio::test"] {
            assert!(
                !attributes.contains(marker),
                "{} has a {marker}: it would run once per test binary — put it in \
                 tests/engine_urls.rs",
                path.display()
            );
        }
    }
}

// ---------------------------------------------------------------------------
// tests/common's own helpers
// ---------------------------------------------------------------------------

#[test]
fn a_required_url_names_an_engine_that_honours_it() {
    let honoured = common::honoured_test_urls();
    for name in common::required_test_urls() {
        assert!(
            honoured.contains(&name.as_str()),
            "{REQUIRE_TEST_URLS} lists {name}, which only {honoured:?} honour — \
             requiring it would check nothing"
        );
    }
}

#[test]
fn a_declared_pg_major_must_match_the_server() {
    assert_eq!(pg_major_mismatch("17", "170006"), None);
    assert_eq!(pg_major_mismatch("18", "180000"), None);
    assert_eq!(pg_major_mismatch(" 18\n", "180001"), None);
    assert!(pg_major_mismatch("18", "170006").is_some());
    assert!(pg_major_mismatch("17", "180000").is_some());
    // Not "1": a prefix comparison would let 17 pass as 1.
    assert!(pg_major_mismatch("1", "170006").is_some());
    assert!(pg_major_mismatch("eighteen", "180000").is_some());
    assert!(pg_major_mismatch("18", "").is_some());
}

#[test]
fn ci_linux_runs_the_postgres_suites() {
    // CI's Linux leg starts a Postgres for the suites (ci.yml), and this is
    // what notices if the URL stops reaching `cargo test` — the step's output
    // not written to $GITHUB_ENV, its `if:` flipped, the step moved below
    // Test, the requirement's name misspelled. Every one of those leaves the
    // Postgres tests skipping and CI green, and every one leaves an `echo`
    // line in ci.yml for a source-reading test to find, which is why this
    // asks the running process instead. Inert everywhere else: `RUNNER_OS`
    // and `GITHUB_ACTIONS` are set by the Actions runner, and a developer
    // running without Postgres should still skip.
    let on_actions = std::env::var("GITHUB_ACTIONS").is_ok_and(|v| v == "true");
    let on_linux = std::env::var("RUNNER_OS").is_ok_and(|v| v == "Linux");
    if !(on_actions && on_linux) {
        return;
    }
    assert!(
        Engine::Postgres.is_set(),
        "CI's Linux leg has no {}, so every Postgres suite skips",
        Engine::Postgres.env()
    );
    assert!(
        is_required(Engine::Postgres.env()),
        "CI's Linux leg does not list {} in {REQUIRE_TEST_URLS}, so losing it \
         would skip the Postgres suites and pass",
        Engine::Postgres.env()
    );
    assert!(
        common::pg_test_major().is_some(),
        "CI's Linux leg does not set {PG_TEST_MAJOR}, so a Postgres pass could \
         reach the wrong server version and still pass"
    );
}

#[test]
fn a_rewritten_url_keeps_everything_but_the_database() {
    // The credentials, port and parameters all have to survive, or the suites
    // silently reconnect to the wrong place — or not at all.
    assert_eq!(
        with_database(
            "mssql://sa:Str0ng%21Passw0rd@localhost:14333/master?encrypt=on&trustServerCertificate=true",
            "hubro_test_1_2"
        ),
        "mssql://sa:Str0ng%21Passw0rd@localhost:14333/hubro_test_1_2?encrypt=on&trustServerCertificate=true"
    );
    // Percent-encoded userinfo passes through untouched. The database is
    // located as the first `/` after the scheme, which is correct precisely
    // because userinfo may not contain a bare `/` — a password that does has
    // to be encoded, and would not survive sqlx or tiberius either. This case
    // is here because the naive reading of "the first slash" looks wrong until
    // you check that.
    assert_eq!(
        with_database("postgres://u:a%2Fb@h:5432/demo", "db"),
        "postgres://u:a%2Fb@h:5432/db"
    );
    assert_eq!(
        with_database(
            "postgres://tester:testpass@localhost:5433/demo",
            "hubro_test_1_2"
        ),
        "postgres://tester:testpass@localhost:5433/hubro_test_1_2"
    );
    // No path at all, and — the case the first version of this test claimed
    // to cover and didn't — no path *with* a query, where the database was
    // being appended inside the last parameter value.
    assert_eq!(
        with_database("postgres://u@h:5432", "db"),
        "postgres://u@h:5432/db"
    );
    assert_eq!(
        with_database(
            "mssql://sa:p@h:14333?encrypt=on&trustServerCertificate=true",
            "db"
        ),
        "mssql://sa:p@h:14333/db?encrypt=on&trustServerCertificate=true"
    );
    // An IPv6 literal host keeps its brackets and colons.
    assert_eq!(
        with_database("postgres://u@[::1]:5432/demo", "db"),
        "postgres://u@[::1]:5432/db"
    );
    // A `/` inside a query value is not the start of a path. `sslrootcert`
    // and friends are stock sqlx parameters holding absolute paths, and
    // reading that slash as the path both misplaced the database *and*
    // truncated the query.
    assert_eq!(
        with_database("postgres://u:p@h:5432?sslrootcert=/etc/ssl/root.crt", "db"),
        "postgres://u:p@h:5432/db?sslrootcert=/etc/ssl/root.crt"
    );
    assert_eq!(
        with_database(
            "postgres://u:p@h:5432/demo?sslrootcert=/etc/ssl/root.crt",
            "db"
        ),
        "postgres://u:p@h:5432/db?sslrootcert=/etc/ssl/root.crt"
    );
    assert_eq!(
        with_database("postgres://u@h:5432/?sslmode=disable", "db"),
        "postgres://u@h:5432/db?sslmode=disable"
    );
}
