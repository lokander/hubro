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
//!
//! The scan covers `tests/common` too, except the body of its one reader,
//! `url_from_env`. Last, the SQL Server suite's catalog turn is pinned here
//! because it rests on the same kind of fact: where a URL may come from.

mod common;

use common::{
    is_required, pg_major_mismatch, with_database, Engine, PG_TEST_MAJOR, REQUIRE_TEST_URLS,
};

// ---------------------------------------------------------------------------
// Reading source as tokens
// ---------------------------------------------------------------------------

/// One token of Rust source. Only the distinctions the rules below need are
/// kept: identifiers, string literals (by content, raw or not), and
/// everything else by its text, with `::` as one token.
#[derive(Debug, Clone, PartialEq)]
enum Tok {
    Ident(String),
    Str(String),
    Other(String),
}

/// `src` as tokens, each with its line, comments dropped.
///
/// Tokens rather than text, so the rules hold however the code is spaced or
/// wrapped (`env :: var` is the same path as `env::var`) and nothing inside a
/// comment or a string counts as code. Handles `"…"` with escapes, raw and
/// byte strings, nested block comments, and tells a character literal from a
/// lifetime.
fn tokens(src: &str) -> Vec<(Tok, usize)> {
    let chars: Vec<char> = src.chars().collect();
    let mut out = Vec::new();
    let mut line = 1;
    let mut i = 0;
    let count_lines =
        |from: usize, to: usize| chars[from..to].iter().filter(|c| **c == '\n').count();
    while i < chars.len() {
        let c = chars[i];
        let next = chars.get(i + 1).copied();
        let start = i;
        if c == '\n' {
            line += 1;
            i += 1;
        } else if c.is_whitespace() {
            i += 1;
        } else if c == '/' && next == Some('/') {
            while i < chars.len() && chars[i] != '\n' {
                i += 1;
            }
        } else if c == '/' && next == Some('*') {
            let mut depth = 0;
            while i < chars.len() {
                if chars[i] == '/' && chars.get(i + 1) == Some(&'*') {
                    depth += 1;
                    i += 2;
                } else if chars[i] == '*' && chars.get(i + 1) == Some(&'/') {
                    depth -= 1;
                    i += 2;
                    if depth == 0 {
                        break;
                    }
                } else {
                    i += 1;
                }
            }
            line += count_lines(start, i);
        } else if c.is_alphabetic() || c == '_' {
            while i < chars.len() && (chars[i].is_alphanumeric() || chars[i] == '_') {
                i += 1;
            }
            let word: String = chars[start..i].iter().collect();
            let raw =
                matches!(word.as_str(), "r" | "br") && matches!(chars.get(i), Some('"' | '#'));
            if raw {
                let mut hashes = 0;
                while chars.get(i) == Some(&'#') {
                    hashes += 1;
                    i += 1;
                }
                if chars.get(i) != Some(&'"') {
                    out.push((Tok::Ident(word), line));
                    continue;
                }
                let open = i + 1;
                let mut end = open;
                while end < chars.len()
                    && !(chars[end] == '"'
                        && (1..=hashes).all(|h| chars.get(end + h) == Some(&'#')))
                {
                    end += 1;
                }
                out.push((Tok::Str(chars[open..end].iter().collect()), line));
                i = (end + 1 + hashes).min(chars.len());
                line += count_lines(start, i);
            } else if word == "b" && chars.get(i) == Some(&'"') {
                // A byte string: the `b` is a prefix, the string follows.
                continue;
            } else {
                out.push((Tok::Ident(word), line));
            }
        } else if c == '"' {
            i += 1;
            while i < chars.len() && chars[i] != '"' {
                i += if chars[i] == '\\' { 2 } else { 1 };
            }
            let end = i.min(chars.len());
            out.push((Tok::Str(chars[start + 1..end].iter().collect()), line));
            i = (end + 1).min(chars.len());
            line += count_lines(start, i);
        } else if c == '\'' {
            // `'\x'` and `'c'` are literals; `'a` is a lifetime.
            let close = if next == Some('\\') {
                chars[i + 2..]
                    .iter()
                    .position(|c| *c == '\'')
                    .map(|p| i + 2 + p)
            } else {
                (chars.get(i + 2) == Some(&'\'')).then_some(i + 2)
            };
            match close {
                Some(close) => i = close + 1,
                None => {
                    i += 1;
                    while i < chars.len() && (chars[i].is_alphanumeric() || chars[i] == '_') {
                        i += 1;
                    }
                }
            }
            out.push((Tok::Other(chars[start..i].iter().collect()), line));
        } else if c == ':' && next == Some(':') {
            out.push((Tok::Other("::".into()), line));
            i += 2;
        } else if c.is_ascii_digit() {
            while i < chars.len() && (chars[i].is_alphanumeric() || chars[i] == '_') {
                i += 1;
            }
            out.push((Tok::Other(chars[start..i].iter().collect()), line));
        } else {
            out.push((Tok::Other(c.to_string()), line));
            i += 1;
        }
    }
    out
}

fn is_ident(tok: Option<&Tok>, name: &str) -> bool {
    matches!(tok, Some(Tok::Ident(word)) if word == name)
}

fn is_punct(tok: Option<&Tok>, text: &str) -> bool {
    matches!(tok, Some(Tok::Other(other)) if other == text)
}

/// The index of the token closing the bracket opened at `open`.
fn matching(toks: &[Tok], open: usize) -> usize {
    let (opener, closer) = match &toks[open] {
        Tok::Other(o) if o == "(" => ("(", ")"),
        Tok::Other(o) if o == "{" => ("{", "}"),
        Tok::Other(o) if o == "[" => ("[", "]"),
        other => panic!("{other:?} opens nothing"),
    };
    let mut depth = 0;
    for (at, tok) in toks.iter().enumerate().skip(open) {
        if is_punct(Some(tok), opener) {
            depth += 1;
        } else if is_punct(Some(tok), closer) {
            depth -= 1;
            if depth == 0 {
                return at;
            }
        }
    }
    toks.len() - 1
}

/// The `const NAME: &str = "…";` items in `toks`, by name, so a read through
/// a constant is judged by the variable it really names.
fn string_consts(toks: &[Tok]) -> Vec<(String, String)> {
    let mut consts = Vec::new();
    for at in 0..toks.len() {
        if !is_ident(toks.get(at), "const") {
            continue;
        }
        let Some(Tok::Ident(name)) = toks.get(at + 1) else {
            continue;
        };
        let rest = &toks[at + 2..];
        let Some(eq) = rest.iter().take(6).position(|t| is_punct(Some(t), "=")) else {
            continue;
        };
        if let (Some(Tok::Str(value)), true) = (rest.get(eq + 1), is_punct(rest.get(eq + 2), ";")) {
            consts.push((name.clone(), value.clone()));
        }
    }
    consts
}

/// The token range of `fn name`'s body, if `toks` defines it.
fn fn_body(toks: &[Tok], name: &str) -> Option<std::ops::Range<usize>> {
    let at = (0..toks.len())
        .find(|&at| is_ident(toks.get(at), "fn") && is_ident(toks.get(at + 1), name))?;
    let open = (at..toks.len()).find(|&i| is_punct(toks.get(i), "{"))?;
    Some(open..matching(toks, open) + 1)
}

/// Every way `src` reads a test URL from the environment itself, one line
/// each, empty when it reads none. Tokens in `exempt` are not looked at.
///
/// Reading the environment is allowed; the rule is about *which* variable,
/// and it is decided on the call, not on how its path is spelled:
///
/// - any call whose final path segment is `var` or `var_os`, whatever comes
///   before it (`std::env::`, `env::`, an alias, nothing after a glob
///   import), and any `env!`/`option_env!`, must have as its argument a
///   string literal, or a constant in the same file holding one, that names
///   no variable ending in `_TEST_URL`. A helper taking the name as a parameter (five suites
///   had one) has a variable there, so it is reported;
/// - a path ending in `var`, `var_os`, `vars` or `vars_os` that is not such a
///   call, a function value such as `.map(std::env::var)`, is reported (a
///   `use` that imports one is not: the calls it enables are judged);
/// - a `use` that renames `env` or anything under it, or globs it, is
///   reported, because the calls it enables could not be told apart.
fn url_reads(src: &str, exempt: Option<std::ops::Range<usize>>) -> Vec<String> {
    let toks_with_lines = tokens(src);
    let toks: Vec<Tok> = toks_with_lines.iter().map(|(t, _)| t.clone()).collect();
    let consts = string_consts(&toks);
    let is_plain = |name: &str| {
        !name.ends_with("_TEST_URL") && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
    };
    let names_a_test_url = |argument: &[Tok]| match argument {
        [Tok::Str(name)] => !is_plain(name),
        // Every same-named constant is checked, not the first: a shadowing
        // `const` in an inner scope may be the one the call really reads.
        [Tok::Ident(constant)] => {
            let values: Vec<&str> = consts
                .iter()
                .filter(|(name, _)| name == constant)
                .map(|(_, value)| value.as_str())
                .collect();
            values.is_empty() || !values.into_iter().all(is_plain)
        }
        _ => true,
    };
    let exempt = exempt.unwrap_or(0..0);
    let uses: Vec<std::ops::Range<usize>> = (0..toks.len())
        .filter(|&at| is_ident(toks.get(at), "use"))
        .map(|at| {
            at..(at..toks.len())
                .find(|&i| is_punct(toks.get(i), ";"))
                .unwrap_or(toks.len())
        })
        .collect();
    let in_a_use = |at: usize| uses.iter().any(|range| range.contains(&at));
    let mut found = Vec::new();
    for (at, (tok, line)) in toks_with_lines.iter().enumerate() {
        if exempt.contains(&at) {
            continue;
        }
        let Tok::Ident(word) = tok else { continue };
        let before = at.checked_sub(1).and_then(|b| toks.get(b));
        let after = toks.get(at + 1);
        match word.as_str() {
            "var" | "var_os" if is_punct(after, "(") && !is_ident(before, "fn") => {
                let close = matching(&toks, at + 1);
                if names_a_test_url(&toks[at + 2..close]) {
                    found.push(format!(
                        "line {line}: calls {word}(…) with {:?}",
                        &toks[at + 2..close]
                    ));
                }
            }
            "var" | "var_os" | "vars" | "vars_os" if is_punct(before, "::") && !in_a_use(at) => {
                found.push(format!("line {line}: names ::{word} other than as a call"));
            }
            "env" | "option_env" if is_punct(after, "!") && is_punct(toks.get(at + 2), "(") => {
                let close = matching(&toks, at + 2);
                if names_a_test_url(&toks[at + 3..close]) {
                    found.push(format!(
                        "line {line}: {word}! with {:?}",
                        &toks[at + 3..close]
                    ));
                }
            }
            "use" => {
                let end = (at..toks.len())
                    .find(|&i| is_punct(toks.get(i), ";"))
                    .unwrap_or(toks.len());
                let statement = &toks[at..end];
                for (offset, tok) in statement.iter().enumerate() {
                    if !is_ident(Some(tok), "env") {
                        continue;
                    }
                    let rest = &statement[offset + 1..];
                    let renames_or_globs = rest
                        .iter()
                        .take_while(|t| !is_punct(Some(t), ","))
                        .any(|t| is_ident(Some(t), "as") || is_punct(Some(t), "*"))
                        || (is_punct(rest.first(), "::") && is_punct(rest.get(1), "{") && {
                            let group = &rest[1..=matching(rest, 1)];
                            group
                                .iter()
                                .any(|t| is_ident(Some(t), "as") || is_punct(Some(t), "*"))
                        });
                    if renames_or_globs {
                        found.push(format!("line {line}: a `use` renames or globs std::env"));
                    }
                }
            }
            _ => {}
        }
    }
    found
}

fn raw_url_reads(src: &str) -> Vec<String> {
    url_reads(src, None)
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

/// The one function body exempt from the scan: `tests/common`'s
/// `url_from_env`, through which every lookup there reads its variable.
const EXEMPT_FN: (&str, &str) = ("mod.rs", "url_from_env");

#[test]
fn no_suite_reads_a_test_url_from_the_environment_itself() {
    let common = tests_dir().join("common");
    let mut offenders = Vec::new();
    let files = rust_files(&tests_dir());
    let mut exempted = 0;
    for path in &files {
        let src = std::fs::read_to_string(path).unwrap();
        let exempt = (path.parent() == Some(common.as_path())
            && path.file_name().is_some_and(|name| name == EXEMPT_FN.0))
        .then(|| {
            let toks: Vec<Tok> = tokens(&src).into_iter().map(|(t, _)| t).collect();
            fn_body(&toks, EXEMPT_FN.1)
        })
        .flatten();
        exempted += usize::from(exempt.is_some());
        for read in url_reads(&src, exempt) {
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
    assert_eq!(
        exempted, 1,
        "tests/common/{} no longer defines fn {}, so nothing should be exempt",
        EXEMPT_FN.0, EXEMPT_FN.1
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
        // Spacing is not spelling.
        r#"fn u() { std::env :: var ("HUBRO_PG_TEST_URL") }"#,
        // A helper taking the name is the shape five suites had.
        "fn url(var: &str) -> Option<String> { std::env::var(var).ok() }",
        // A constant is judged by what it holds.
        "const V: &str = \"HUBRO_PG_TEST_URL\";\nfn u() { std::env::var(V) }",
        "const V: &str = \"HUBRO_X\";\nfn u() { const V: &str = \"HUBRO_PG_TEST_URL\"; std::env::var(V) }",
        "fn u() { std::env::var(format!(\"HUBRO_{}_TEST_URL\", 1)) }",
        "use std::env::var;\nfn u() { var(\"HUBRO_PG_TEST_URL\") }",
        "use std::env as e;\nfn u() { e::var(\"HUBRO_PG_TEST_URL\") }",
        "use std::{env as e};",
        "use std::env::*;\nfn u() { var(\"HUBRO_PG_TEST_URL\") }",
        "use std::env::{var as v};",
        "fn u() { std::env::vars().count() }",
        "fn u() { [\"X\"].map(std::env::var) }",
        r#"const U: Option<&str> = option_env!("HUBRO_PG_TEST_URL");"#,
        // A `//` inside a string does not start a comment.
        r#"fn u() { let a = "postgres://x"; std::env::var("HUBRO_PG_TEST_URL") }"#,
        // Nor does a lifetime start a character literal that hides the call.
        r#"fn u<'a>(x: &'a str) { std::env::var("HUBRO_PG_TEST_URL"); }"#,
        r#"fn u() { let q = '\''; let b = b'"'; std::env::var("HUBRO_PG_TEST_URL"); }"#,
    ] {
        assert!(!raw_url_reads(raw).is_empty(), "missed: {raw}");
    }
    for clean in [
        r#"fn u() { std::env::var("HUBRO_SSH_TEST_HOST") }"#,
        "const V: &str = \"HUBRO_PERF_REBUILD\";\nfn u() { std::env::var_os(V) }",
        r#"fn u() { env!("CARGO_MANIFEST_DIR") }"#,
        "use std::{env, io::Write as _};",
        // Importing the function is not a read; its calls are judged.
        "use std::env::{self, var_os};\nfn u() { var_os(\"HUBRO_PERF_REBUILD\") }",
        // The flag variable is not a URL variable.
        r#"fn u() { std::env::var("HUBRO_REQUIRE_TEST_URLS") }"#,
        "fn var(x: u8) {}",
        "// std::env::var(\"HUBRO_PG_TEST_URL\") in a comment",
        "/* std::env::var(var) /* nested */ still a comment */",
        r#"fn u() { let s = "std::env::var(var)"; }"#,
        r##"fn u() { let s = r#"std::env::var("HUBRO_PG_TEST_URL")"#; }"##,
        r#"fn u() { let q = '"'; common::engine_url("HUBRO_CITUS_TEST_URL") }"#,
        r##"fn u() { let s = br#"\" std::env::var(var)"#; }"##,
    ] {
        assert_eq!(raw_url_reads(clean), Vec::<String>::new(), "{clean}");
    }
    // The exemption covers the one body and not the function beside it.
    let src =
        "fn url_from_env(v: &str) { std::env::var(v) }\nfn other(v: &str) { std::env::var(v) }";
    let toks: Vec<Tok> = tokens(src).into_iter().map(|(t, _)| t).collect();
    let found = url_reads(src, fn_body(&toks, "url_from_env"));
    assert_eq!(found.len(), 1, "{found:?}");
    assert!(found[0].starts_with("line 2:"), "{found:?}");
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
        let toks: Vec<Tok> = tokens(&src).into_iter().map(|(t, _)| t).collect();
        for at in 0..toks.len() {
            let attribute = is_punct(toks.get(at), "#") && is_punct(toks.get(at + 1), "[");
            let names_test = is_ident(toks.get(at + 2), "test")
                || (is_ident(toks.get(at + 2), "tokio")
                    && is_punct(toks.get(at + 3), "::")
                    && is_ident(toks.get(at + 4), "test"));
            assert!(
                !(attribute && names_test),
                "{} has a test attribute: it would run once per test binary — put it \
                 in tests/engine_urls.rs",
                path.display()
            );
        }
    }
}

/// The name of the innermost `fn` whose body holds token `at`, if any.
fn enclosing_fn(toks: &[Tok], at: usize) -> Option<String> {
    let mut best: Option<(usize, String)> = None;
    for start in 0..toks.len() {
        if !is_ident(toks.get(start), "fn") {
            continue;
        }
        let Some(Tok::Ident(name)) = toks.get(start + 1) else {
            continue;
        };
        let Some(open) =
            (start..toks.len()).find(|&i| is_punct(toks.get(i), "{") || is_punct(toks.get(i), ";"))
        else {
            continue;
        };
        if !is_punct(toks.get(open), "{") {
            continue;
        }
        let body = open..matching(toks, open) + 1;
        if body.contains(&at) && best.as_ref().is_none_or(|(b, _)| *b < body.start) {
            best = Some((body.start, name.clone()));
        }
    }
    best.map(|(_, name)| name)
}

/// Whether a call to `introspect` starts at token `at`, as a method
/// (`pool.introspect()`) or by path (`DbPool::introspect(pool)`).
fn calls_introspect(toks: &[Tok], at: usize) -> bool {
    (is_punct(toks.get(at), ".") || is_punct(toks.get(at), "::"))
        && is_ident(toks.get(at + 1), "introspect")
        && is_punct(toks.get(at + 2), "(")
}

#[test]
fn an_introspect_call_is_found_however_it_is_spelled() {
    for (src, calls) in [
        ("pool.introspect().await", true),
        ("DbPool::introspect(&pool).await", true),
        ("pool . introspect ( )", true),
        ("introspect_table(&mut turn, &pool, name)", false),
        ("fn introspect() {}", false),
    ] {
        let toks: Vec<Tok> = tokens(src).into_iter().map(|(t, _)| t).collect();
        let found = (0..toks.len()).any(|at| calls_introspect(&toks, at));
        assert_eq!(found, calls, "{src}");
    }
}

#[test]
fn tests_common_never_introspects_on_its_own() {
    // How a suite may introspect is the suite's business: SQL Server's must
    // trade its catalog turn for the exclusive half first. A shared helper
    // that called `introspect()` itself would do it under whatever its caller
    // held, which review found one doing. So helpers take the metadata, or a
    // closure that fetches it.
    for path in rust_files(&tests_dir().join("common")) {
        let src = std::fs::read_to_string(&path).unwrap();
        let toks: Vec<Tok> = tokens(&src).into_iter().map(|(t, _)| t).collect();
        for at in 0..toks.len() {
            assert!(
                !calls_introspect(&toks, at),
                "{} calls introspect() in {:?}: take the TableMeta, or an introspect \
                 closure, from the suite instead",
                path.display(),
                enclosing_fn(&toks, at)
            );
        }
    }
}

#[test]
fn only_the_url_lookups_read_through_url_from_env() {
    // `url_from_env` is exempt from the scan, so whatever calls it is a road
    // to a raw URL. These are the roads that apply the checks (the
    // per-engine refusals and `HUBRO_REQUIRE_TEST_URLS`), plus `is_set`,
    // which hands back only whether the variable is there.
    let src = std::fs::read_to_string(tests_dir().join("common").join("mod.rs")).unwrap();
    let toks: Vec<Tok> = tokens(&src).into_iter().map(|(t, _)| t).collect();
    let mut callers: Vec<String> = (0..toks.len())
        .filter(|&at| {
            is_ident(toks.get(at), "url_from_env")
                && is_punct(toks.get(at + 1), "(")
                && !is_ident(at.checked_sub(1).and_then(|b| toks.get(b)), "fn")
        })
        .map(|at| enclosing_fn(&toks, at).unwrap_or_else(|| "(no fn)".into()))
        .collect();
    callers.sort();
    assert_eq!(callers, ["engine_url", "is_set", "per_process_database"]);
    // Named nowhere else either: passed as a value, it could be called from
    // anywhere.
    let mentions = (0..toks.len())
        .filter(|&at| is_ident(toks.get(at), "url_from_env"))
        .count();
    assert_eq!(
        mentions, 4,
        "url_from_env is named outside its definition and three calls"
    );
}

// ---------------------------------------------------------------------------
// The SQL Server suite's catalog turn
// ---------------------------------------------------------------------------

#[test]
fn every_sqlserver_test_holds_its_catalog_turn_until_it_ends() {
    // db_sqlserver.rs keeps introspection apart from everything else by
    // having each test hold a shared turn for its whole run (see `CATALOG`
    // there). That holds only while the URL comes from nowhere but
    // `session()`, which takes the turn, `introspect()` is called from
    // nowhere but `introspect_table`, which trades it for the exclusive
    // half, and no test drops its turn by binding it to `_`.
    let src = std::fs::read_to_string(tests_dir().join("db_sqlserver.rs")).unwrap();
    let toks: Vec<Tok> = tokens(&src).into_iter().map(|(t, _)| t).collect();
    let session = fn_body(&toks, "session").expect("db_sqlserver.rs defines fn session");
    let introspect_table =
        fn_body(&toks, "introspect_table").expect("db_sqlserver.rs defines fn introspect_table");
    let takes_the_shared_half = session.clone().any(|at| {
        is_ident(toks.get(at), "CATALOG")
            && is_punct(toks.get(at + 1), ".")
            && is_ident(toks.get(at + 2), "read")
    });
    assert!(
        takes_the_shared_half,
        "session() no longer takes CATALOG.read()"
    );
    let mut calls = 0;
    for at in 0..toks.len() {
        if is_ident(toks.get(at), "mssql_test_url") {
            assert!(
                session.contains(&at),
                "mssql_test_url is read outside session()"
            );
        }
        if calls_introspect(&toks, at) {
            assert!(
                introspect_table.contains(&at),
                "introspect() is called outside introspect_table"
            );
        }
        if is_ident(toks.get(at), "session")
            && is_punct(toks.get(at + 1), "(")
            && !is_ident(at.checked_sub(1).and_then(|b| toks.get(b)), "fn")
        {
            calls += 1;
            // let Some((url, turn)) = session()   — the turn is the last
            // binding before `))` and `=`.
            let binding = at
                .checked_sub(4)
                .and_then(|b| toks.get(b))
                .expect("a binding before session()");
            assert!(
                is_punct(toks.get(at - 1), "=")
                    && is_punct(toks.get(at - 2), ")")
                    && is_punct(toks.get(at - 3), ")")
                    && matches!(binding, Tok::Ident(name) if name != "_" && name != "mut"),
                "a session() call does not bind its turn to a name: {:?}",
                &toks[at.saturating_sub(8)..at]
            );
        }
    }
    assert!(calls > 40, "only {calls} tests take a session");
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
