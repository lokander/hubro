//! Reading Rust source as code, for the tests that pin what the shipped code
//! does by reading it: which text is code rather than a comment or a literal,
//! where a `{ … }` block ends, and what a file holds outside its test modules.
//!
//! One lexer and one "outside the test modules" scanner for every such test.
//! Hand-rolled copies kept being wrong in the same few ways: a file was cut
//! at its *first* `#[cfg(test)] mod`, so a test-only declaration near the top
//! (`#[cfg(test)] mod source_reading;`) hid everything after it; or braces in
//! a doc comment, a format string or a char literal were counted, so a span
//! ended early. Each mistake makes a scan see less code than ships, and a
//! scan that sees less passes.

/// What a byte of source is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Class {
    Code,
    Comment,
    Literal,
}

/// The class of every byte of `text`: inside a comment (line, or block —
/// nested, as Rust's are), inside a literal (a string with escapes, a raw or
/// byte string, a char literal — not a lifetime), or code.
fn classify(text: &str) -> Vec<Class> {
    let chars: Vec<(usize, char)> = text.char_indices().collect();
    let at = |i: usize| chars.get(i).map(|&(_, c)| c);
    let offset = |i: usize| chars.get(i).map_or(text.len(), |&(o, _)| o);
    let mut classes = vec![Class::Code; text.len()];
    let mut mark = |from: usize, to: usize, class: Class| {
        for byte in &mut classes[offset(from)..offset(to)] {
            *byte = class;
        }
    };
    let mut i = 0;
    while i < chars.len() {
        let start = i;
        match chars[i].1 {
            '/' if at(i + 1) == Some('/') => {
                while i < chars.len() && chars[i].1 != '\n' {
                    i += 1;
                }
                mark(start, i, Class::Comment);
                continue;
            }
            '/' if at(i + 1) == Some('*') => {
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
                mark(start, i, Class::Comment);
                continue;
            }
            // A raw string, `r"…"` or `r#"…"#` (`br`/`cr` too: the prefix
            // letter before it is code). An identifier cannot end in `r`
            // directly before a `"` or `#"` in valid Rust, and `r#type` is a
            // raw identifier: no `"` follows its hashes.
            'r' => {
                let mut j = i + 1;
                while at(j) == Some('#') {
                    j += 1;
                }
                if at(j) == Some('"') {
                    let hashes = j - i - 1;
                    j += 1;
                    while j < chars.len()
                        && !(chars[j].1 == '"' && (1..=hashes).all(|k| at(j + k) == Some('#')))
                    {
                        j += 1;
                    }
                    i = (j + hashes + 1).min(chars.len());
                    mark(start, i, Class::Literal);
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
                i = (i + 1).min(chars.len());
                mark(start, i, Class::Literal);
                continue;
            }
            // An escaped char literal: `'\''`, `'\n'`, `'\u{7f}'`.
            '\'' if at(i + 1) == Some('\\') => {
                let mut j = i + 3;
                while at(j).is_some_and(|c| c != '\'') {
                    j += 1;
                }
                i = (j + 1).min(chars.len());
                mark(start, i, Class::Literal);
                continue;
            }
            // A one-character literal: `'{'`, `'"'`. A lifetime (`'a`) has
            // no closing quote two characters on.
            '\'' if at(i + 2) == Some('\'') => {
                i += 3;
                mark(start, i, Class::Literal);
                continue;
            }
            _ => {}
        }
        i += 1;
    }
    classes
}

/// The byte length of `text` up to and including the brace that closes the
/// first `{` in it that is code, or `None` when it never closes.
pub(crate) fn block_end(text: &str) -> Option<usize> {
    let classes = classify(text);
    let mut depth = 0usize;
    for (at, byte) in text.bytes().enumerate() {
        if classes[at] != Class::Code {
            continue;
        }
        match byte {
            b'{' => depth += 1,
            b'}' if depth > 0 => {
                depth -= 1;
                if depth == 0 {
                    return Some(at + 1);
                }
            }
            _ => {}
        }
    }
    None
}

/// `text` without its test modules, otherwise as written, comments
/// included; lines joined with `\n`, so a CRLF checkout reads the same.
///
/// A `#[cfg(test)]` on a `mod x { … }` drops exactly that block,
/// brace-matched past comments and literals; on a `mod x;` declaration, only
/// the declaration. Reading goes on after either, so code following a test
/// module or a test-only declaration is still read. A `#[cfg(test)]` on
/// anything else — a `use`, a function — leaves it in place, and one inside a
/// comment or a string is not an attribute at all.
pub(crate) fn shipped(text: &str) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let joined = lines.join("\n");
    let classes = classify(&joined);
    let mut starts = Vec::with_capacity(lines.len());
    let mut offset = 0;
    for line in &lines {
        starts.push(offset);
        offset += line.len() + 1;
    }
    let is_code = |line: usize| {
        let lead = lines[line].len() - lines[line].trim_start().len();
        classes.get(starts[line] + lead) == Some(&Class::Code)
    };
    let mut kept: Vec<&str> = Vec::with_capacity(lines.len());
    let mut i = 0;
    while i < lines.len() {
        if lines[i].trim() == "#[cfg(test)]" && is_code(i) {
            // Past any further attributes, to the item they are on.
            let mut item = i + 1;
            while item < lines.len()
                && (lines[item].trim().is_empty() || lines[item].trim().starts_with("#["))
            {
                item += 1;
            }
            let head = lines.get(item).map_or("", |line| line.trim());
            let is_mod =
                head.starts_with("mod ") || (head.starts_with("pub") && head.contains(" mod "));
            if is_mod && head.ends_with(';') {
                i = item + 1;
                continue;
            }
            if is_mod {
                // Fails closed: a block that never closes, or closes anywhere
                // but on a `}` of its own at the `mod` line's indentation, is
                // a mis-lex, and reading on from it would silently drop — or
                // keep — the wrong code.
                let len = block_end(&joined[starts[item]..]).unwrap_or_else(|| {
                    panic!("the test module `{head}` on line {} never closes", item + 1)
                });
                let end = starts[item] + len;
                let closing = starts.partition_point(|&start| start < end) - 1;
                let indent = &lines[item][..lines[item].len() - lines[item].trim_start().len()];
                assert_eq!(
                    lines[closing].trim_end(),
                    format!("{indent}}}"),
                    "the test module `{head}` on line {} closes on line {}, not on a `}}` \
                     of its own at its indentation",
                    item + 1,
                    closing + 1
                );
                // Resume on the line after the one the block closes on.
                i = closing + 1;
                continue;
            }
        }
        kept.push(lines[i]);
        i += 1;
    }
    kept.join("\n")
}

/// `text` with every comment removed and literals left as written; line
/// breaks inside a block comment are kept, so lines still line up.
pub(crate) fn without_comments(text: &str) -> String {
    let classes = classify(text);
    text.char_indices()
        .filter(|&(at, c)| classes[at] != Class::Comment || c == '\n')
        .map(|(_, c)| c)
        .collect()
}

/// The code of a Rust file outside its test modules, comments removed.
pub(crate) fn code_outside_test_modules(text: &str) -> String {
    without_comments(&shipped(text))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_test_module_is_dropped_and_what_follows_it_is_read() {
        let after_declaration = "#[cfg(test)]\nmod test_dom;\n\nfn app() { probe(); }\n";
        assert!(shipped(after_declaration).contains("probe()"));
        assert!(!shipped(after_declaration).contains("mod test_dom"));
        let after_module = "fn a() {}\n#[cfg(test)]\nmod tests {\n    fn t() { let s = \"}\"; let c = '{'; hidden(); }\n}\n\nfn later() { probe(); }\n";
        let kept = shipped(after_module);
        assert!(
            kept.contains("probe()") && !kept.contains("hidden()"),
            "{kept}"
        );
        let attributed = "fn a() {}\n#[cfg(test)]\n#[allow(unused)]\npub(crate) mod tests {\n    fn t() { if x { hidden(); } }\n}\nfn b() { probe(); }\n";
        let kept = shipped(attributed);
        assert!(
            kept.contains("probe()") && !kept.contains("hidden()"),
            "{kept}"
        );
        // Not a module, so not dropped; and not an attribute, so not obeyed.
        let not_a_module = "#[cfg(test)]\nuse std::fmt;\nfn a() { probe(); }\n";
        assert_eq!(shipped(not_a_module), not_a_module.trim_end());
        let in_a_string = "const S: &str = \"\n#[cfg(test)]\nmod x {\n\";\nfn a() { probe(); }\n";
        assert!(shipped(in_a_string).contains("probe()"));
        // CRLF reads the same.
        assert!(shipped(&after_module.replace('\n', "\r\n")).contains("probe()"));
        let uncommented =
            code_outside_test_modules("// probe()\nfn a() {} /* probe() */ /// probe()\n");
        assert!(
            uncommented.contains("fn a() {}") && !uncommented.contains("probe"),
            "{uncommented}"
        );
    }

    #[test]
    #[should_panic(expected = "the test module `mod tests {` on line 3 never closes")]
    fn a_test_module_that_never_closes_is_refused() {
        shipped("fn a() {}\n#[cfg(test)]\nmod tests {\n    fn t() {}\n");
    }

    #[test]
    #[should_panic(
        expected = "the test module `mod tests {` on line 3 closes on line 4, \
                               not on a `}` of its own at its indentation"
    )]
    fn a_test_module_that_closes_off_its_indentation_is_refused() {
        shipped("fn a() {}\n#[cfg(test)]\nmod tests {\n    fn t() {} }\nfn b() {}\n");
    }

    #[test]
    fn the_state_module_is_read_past_its_test_only_declarations() {
        // `ui/state/mod.rs` declares a test-only module near its top; cut
        // there, a scan saw 33 of its 3,000-odd lines and passed.
        let state = shipped(include_str!("ui/state/mod.rs"));
        assert!(state.contains("pub struct AppState"), "{state}");
        assert!(state.contains("pub fn tab_title("));
        assert!(!state.contains("fn the_reading_scan_reads_past_test_modules"));
        let ui = shipped(include_str!("ui/mod.rs"));
        assert!(ui.lines().count() > 30, "{ui}");
    }

    #[test]
    fn a_brace_in_a_comment_or_a_literal_does_not_end_a_block() {
        // Each of these, counted as code, ends the block early or runs it on:
        // a doc comment, a line comment, a nested block comment, a format
        // string, an escaped quote, raw strings with a `"` inside, a byte raw
        // string ending in a backslash, and char literals plain and escaped.
        // A lifetime is not a char literal.
        let source = r####"
fn first<'a>(x: &'a str) -> &'a str {
    /// a doc line with an open brace {
    // and a line comment with a second open brace {
    /* a block comment } /* nested { */ still } */
    let s = format!("{{ {x} }} }}");
    let q = "a quote \" then a brace }";
    let raw = r#"one " quote }"#;
    let path = br"C:\"; let s = "}";
    let c = '}';
    let o = '{';
    let e = '\'';
    let d = '\"';
    let close = '\u{7d}';
    if x.is_empty() {
        return x;
    }
    x
}

fn second() {}
"####;
        let end = block_end(source).expect("the block closes");
        let block = &source[..end];
        let last: Vec<&str> = block.lines().rev().take(2).collect();
        assert_eq!(last, ["}", "    x"], "{block}");
        assert!(!block.contains("fn second"), "{block}");
        let source = "fn f() { for r in v { let ptr = \"}\"; } let r#type = 1; }\nfn g() {}";
        assert_eq!(
            &source[..block_end(source).unwrap()],
            "fn f() { for r in v { let ptr = \"}\"; } let r#type = 1; }"
        );
    }
}
