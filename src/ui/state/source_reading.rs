//! Helpers for the tests that read source: finding a function's body,
//! stripping comments, counting identifiers. Shared by the test modules of
//! the files the connect flows are split across.

/// The shipped code of the four files the connect flows are split across —
/// `connect.rs`, `secrets_policy.rs`, `backend.rs` and `saved.rs` — one after
/// another, test modules left out. A test that pins a function by its
/// signature reads this, so moving the function between those files is not a
/// failure, and a scan over "the connect code" covers all four. The test
/// modules are left out because a test naming a signature in a string would
/// otherwise be found before the function it names.
pub(super) fn connect_code() -> String {
    [
        include_str!("connect.rs"),
        include_str!("secrets_policy.rs"),
        include_str!("backend.rs"),
        include_str!("saved.rs"),
    ]
    .map(shipped)
    .join("\n")
}

/// The text from `signature` to the brace that closes the body it opens —
/// a method, a function, or any `{ … }` block a test wants to read — by brace
/// matching that skips comments and literals.
///
/// Braces inside a comment, a string (raw or not) or a char literal are not
/// code: counted, an unbalanced one in a doc comment or a format string would
/// end the span early or run it on into the next function, and every
/// assertion against that span would then read the wrong code — failing with
/// a message that blames a "missing" call.
pub(super) fn method_body(source: &str, signature: &str) -> String {
    let from = source
        .find(signature)
        .unwrap_or_else(|| panic!("no `{signature}` in the source read"));
    let end = body_end(&source[from..])
        .unwrap_or_else(|| panic!("unbalanced braces after `{signature}`"));
    source[from..from + end].to_string()
}

/// The byte length of `text` up to and including the brace that closes the
/// first `{` in it that is code, or `None` when it never closes.
fn body_end(text: &str) -> Option<usize> {
    let chars: Vec<(usize, char)> = text.char_indices().collect();
    let at = |i: usize| chars.get(i).map(|&(_, c)| c);
    let is_ident = |c: Option<char>| c.is_some_and(|c| c.is_alphanumeric() || c == '_');
    let mut depth = 0usize;
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i].1;
        match c {
            '/' if at(i + 1) == Some('/') => {
                while i < chars.len() && chars[i].1 != '\n' {
                    i += 1;
                }
                continue;
            }
            '/' if at(i + 1) == Some('*') => {
                // Block comments nest in Rust.
                let mut nested = 0usize;
                while i < chars.len() {
                    if chars[i].1 == '/' && at(i + 1) == Some('*') {
                        nested += 1;
                        i += 2;
                    } else if chars[i].1 == '*' && at(i + 1) == Some('/') {
                        nested -= 1;
                        i += 2;
                        if nested == 0 {
                            break;
                        }
                    } else {
                        i += 1;
                    }
                }
                continue;
            }
            // A raw string: `r"…"`, `r#"…"#`, `br#"…"#` — but not the `r` that
            // ends an identifier such as `for` or `ptr`.
            'r' if !is_ident(i.checked_sub(1).and_then(at))
                || (i >= 1
                    && at(i - 1) == Some('b')
                    && !is_ident(i.checked_sub(2).and_then(at))) =>
            {
                let mut j = i + 1;
                while at(j) == Some('#') {
                    j += 1;
                }
                if at(j) == Some('"') {
                    let hashes = j - i - 1;
                    j += 1;
                    loop {
                        match at(j) {
                            None => return None,
                            Some('"') if (1..=hashes).all(|k| at(j + k) == Some('#')) => {
                                i = j + hashes + 1;
                                break;
                            }
                            _ => j += 1,
                        }
                    }
                    continue;
                }
            }
            '"' => {
                i += 1;
                while i < chars.len() && chars[i].1 != '"' {
                    if chars[i].1 == '\\' {
                        i += 1;
                    }
                    i += 1;
                }
                i += 1;
                continue;
            }
            // A char literal — `'{'`, `'"'`, `'\''` — rather than a lifetime.
            '\'' if at(i + 1) == Some('\\') => {
                let mut j = i + 3;
                while at(j).is_some_and(|c| c != '\'') {
                    j += 1;
                }
                i = j + 1;
                continue;
            }
            '\'' if at(i + 2) == Some('\'') => {
                i += 3;
                continue;
            }
            '{' => depth += 1,
            '}' if depth > 0 => {
                depth -= 1;
                if depth == 0 {
                    return Some(chars[i].0 + 1);
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

/// The code of a function with its comment lines removed — the prose
/// around these decisions discusses what is deliberately *not* stored, and
/// would otherwise match every pattern the sweep below looks for.
pub(super) fn code_of(body: &str) -> String {
    body.lines()
        .filter(|line| !line.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// `code` with every run of whitespace collapsed to a single space, so a
/// pinned line still matches once rustfmt wraps it. Matching the line as
/// typed instead turns a reformat into a failure claiming the decision was
/// severed from its policy — wrong, and alarming to whoever hits it.
pub(super) fn collapsed(code: &str) -> String {
    code.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The first tracked identifier that appears negated in `code`, skipping
/// `!=` and macro bangs. Receivers are stepped over, so `!prompt.remember`
/// and `!*remember.peek()` are found as readily as `!remember`.
pub(super) fn negated(code: &str, names: &[&str]) -> Option<String> {
    code.match_indices('!').find_map(|(at, _)| {
        let rest = code[at + 1..].trim_start_matches(['*', '&', '(']);
        let rest = ["prompt.", "self.", "state.", "acquired."]
            .iter()
            .find_map(|receiver| rest.strip_prefix(receiver))
            .unwrap_or(rest);
        names
            .iter()
            .find(|name| rest.starts_with(**name))
            .map(|name| format!("!{name}"))
    })
}

/// The calls of `macro_name` (`"format!("`, say) in `code`, each as the
/// text between its parentheses.
pub(super) fn macro_calls(code: &str, macro_name: &str) -> Vec<String> {
    let mut calls = Vec::new();
    for (at, _) in code.match_indices(macro_name) {
        let preceded = code[..at]
            .chars()
            .next_back()
            .is_some_and(|c| c.is_alphanumeric() || c == '_');
        if preceded {
            continue;
        }
        let body = &code[at + macro_name.len()..];
        let (mut depth, mut in_str, mut escaped) = (1usize, false, false);
        for (offset, ch) in body.char_indices() {
            match (in_str, ch) {
                (true, '\\') if !escaped => {
                    escaped = true;
                    continue;
                }
                (true, '"') if !escaped => in_str = false,
                (false, '"') => in_str = true,
                (false, '(') => depth += 1,
                (false, ')') => {
                    depth -= 1;
                    if depth == 0 {
                        calls.push(body[..offset].to_string());
                        break;
                    }
                }
                _ => {}
            }
            escaped = false;
        }
    }
    calls
}

/// What of a macro call can name a binding: the code outside its string
/// literals, and the `{…}` placeholders inside them. A literal's own words
/// ("token endpoint") are not a value being formatted.
pub(super) fn formatted_names(call: &str) -> String {
    let (mut out, mut in_str, mut in_hole, mut escaped) = (String::new(), false, false, false);
    for ch in call.chars() {
        if in_str {
            match ch {
                '\\' if !escaped => {
                    escaped = true;
                    continue;
                }
                '"' if !escaped => in_str = false,
                '{' => in_hole = true,
                '}' => {
                    in_hole = false;
                    out.push(' ');
                }
                _ if in_hole => out.push(ch),
                _ => {}
            }
            escaped = false;
        } else if ch == '"' {
            in_str = true;
        } else {
            out.push(ch);
        }
    }
    out
}

/// `line` without a trailing `//` comment. A `//` inside a string literal
/// (a URL) is kept: it counts as a comment only where the quotes before it
/// are balanced. Approximate — escaped quotes and the `'"'` char literal
/// are skipped, but quotes are tracked per line, so a continuation line of
/// a multi-line string that holds `//` is cut there.
///
/// **It fails open.** Cutting too much drops real code, which weakens the
/// must-not-contain checks it feeds (nothing outside the gate, no message
/// text under `src/ui`) instead of tripping them; the ordered and verbatim
/// pins would fail loudly. Enough for the shipped code it reads today.
pub(super) fn strip_trailing_comment(line: &str) -> &str {
    let bytes = line.as_bytes();
    let mut in_string = false;
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' if in_string => i += 1,
            b'\'' if !in_string && bytes.get(i + 1..i + 3) == Some(b"\"'") => i += 2,
            b'"' => in_string = !in_string,
            b'/' if !in_string && bytes.get(i + 1) == Some(&b'/') => return &line[..i],
            _ => {}
        }
        i += 1;
    }
    line
}

/// `source` without comments — whole comment lines and trailing ones — so
/// a test reading code cannot be satisfied, or tripped, by prose that names
/// what it looks for.
pub(super) fn code_only(source: &str) -> String {
    source
        .lines()
        .map(strip_trailing_comment)
        .filter(|line| !line.trim().is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

/// `source` up to its test module (`#[cfg(test)]` followed by `mod`): the
/// shipped code alone. A `#[cfg(test)]` on anything else — `cli.rs` has
/// one on a `use` — does not end it. Line-based, so a CRLF checkout reads
/// the same.
pub(super) fn shipped(source: &str) -> String {
    let lines: Vec<&str> = source.lines().collect();
    let end = lines
        .windows(2)
        .position(|w| w[0].trim() == "#[cfg(test)]" && w[1].trim_start().starts_with("mod "))
        .unwrap_or(lines.len());
    lines[..end].join("\n")
}

/// `code` with the contents of every `"…"` literal removed (escapes
/// honoured), so counting identifiers does not count words in messages.
pub(super) fn without_string_literals(code: &str) -> String {
    let mut out = String::new();
    let (mut in_string, mut escaped) = (false, false);
    for ch in code.chars() {
        if in_string {
            match (escaped, ch) {
                (true, _) => escaped = false,
                (false, '\\') => escaped = true,
                (false, '"') => {
                    in_string = false;
                    out.push(ch);
                }
                _ => {}
            }
        } else {
            in_string = ch == '"';
            out.push(ch);
        }
    }
    out
}

/// How often `ident` occurs in `code` as a whole identifier — `retry` in
/// `Some(retry)`, not in `password_retry`.
pub(super) fn ident_count(code: &str, ident: &str) -> usize {
    let is_ident = |c: char| c.is_alphanumeric() || c == '_';
    code.match_indices(ident)
        .filter(|(at, _)| {
            let before = code[..*at].chars().next_back();
            let after = code[at + ident.len()..].chars().next();
            !before.is_some_and(is_ident) && !after.is_some_and(is_ident)
        })
        .count()
}

/// How many occurrences of `call` (ending in `(`) in collapsed `code` build
/// a value rather than match one. A pattern is `(_)`, the declaration's
/// `(String)`, or a binding followed by `=>` or `|`.
pub(super) fn constructions_of(code: &str, call: &str) -> usize {
    code.match_indices(call)
        .filter(|(at, _)| {
            let rest = &code[at + call.len()..];
            let Some(close) = rest.find(')') else {
                return true;
            };
            let (arg, after) = (rest[..close].trim(), rest[close + 1..].trim_start());
            let pattern =
                arg == "_" || arg == "String" || after.starts_with("=>") || after.starts_with('|');
            !pattern
        })
        .count()
}

/// Whether `code` rebinds `ident`: `let ident`, `let mut ident`, or an
/// assignment `ident = …` (not `==`). Only those shapes: a binding inside
/// another pattern — a tuple or struct `let`, `if let`, `while let`, a
/// `match` arm, `for`, a closure parameter — is not seen.
pub(super) fn rebinds(code: &str, ident: &str) -> bool {
    let is_ident = |c: char| c.is_alphanumeric() || c == '_';
    code.match_indices(ident).any(|(at, _)| {
        let before = &code[..at];
        let after = &code[at + ident.len()..];
        if before.chars().next_back().is_some_and(is_ident)
            || after.chars().next().is_some_and(is_ident)
        {
            return false;
        }
        before.ends_with("let ")
            || before.ends_with("let mut ")
            || (after.trim_start().starts_with('=') && !after.trim_start().starts_with("=="))
    })
}

/// Every `.rs` file under `src/`, as `(path relative to src, contents)`.
pub(super) fn all_sources() -> Vec<(String, String)> {
    fn walk(dir: &std::path::Path, root: &std::path::Path, out: &mut Vec<(String, String)>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                walk(&path, root, out);
            } else if path.extension().is_some_and(|e| e == "rs") {
                let rel = path
                    .strip_prefix(root)
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/");
                out.push((rel, std::fs::read_to_string(&path).unwrap()));
            }
        }
    }
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut out = Vec::new();
    walk(&root, &root, &mut out);
    assert!(
        out.len() > 20,
        "found only {} source files under {root:?}",
        out.len()
    );
    out
}

#[test]
fn a_brace_in_a_comment_or_a_literal_does_not_end_the_body() {
    // Each of these, counted as code, ends the span early or runs it into the
    // next function: an unbalanced brace in a doc comment, a line comment, a
    // block comment, a format string, an escaped quote before one, a raw
    // string and a char literal. A lifetime is not a char literal.
    let source = r####"
fn first<'a>(x: &'a str) -> &'a str {
    /// a doc line with an open brace {
    // and a line comment with a second open brace {
    /* a block comment } /* nested { */ still } */
    let s = format!("{{ {x} }} }}");
    let q = "a quote \" then a brace }";
    let raw = r#"a "raw" string } with a quote"#;
    let c = '}';
    let o = '{';
    let e = '\'';
    let bytes = br"}";
    if x.is_empty() {
        return x;
    }
    x
}

fn second() {}
"####;
    let body = method_body(source, "fn first<");
    let last: Vec<&str> = body.lines().rev().take(2).collect();
    assert_eq!(last, ["}", "    x"], "{body}");
    assert!(!body.contains("fn second"), "{body}");
    // An identifier ending in `r` is not a raw string's prefix.
    let source = "fn f() { for r in v { let ptr = \"}\"; } }\nfn g() {}";
    assert_eq!(
        method_body(source, "fn f("),
        "fn f() { for r in v { let ptr = \"}\"; } }"
    );
}
