//! Two claims `ci.yml` makes, made checkable: what it declines to run on, and
//! that its Linux leg really runs the Postgres suites.
//!
//! **What it declines to run on**, checked against what the build and the
//! tests actually read.
//!
//! A `paths-ignore` entry is a claim that no change to the files it matches
//! can turn CI red. The claim used to be wrong for every Markdown file:
//! `**/*.md` skipped CI on a docs-only commit, while `support_matrix.rs` reads
//! `README.md`, `file_associations.rs` walks `packaging/` (README included) and
//! `no_issue_ids.rs` scans every tracked text file. A docs commit straight to
//! `main` could break any of them with no check at all, and `main` then went
//! red on the next code push — blamed on the wrong commit. `assets/**/*.ico`
//! was wrong the same way for `assets/favicon.ico`, which `asset!()` makes a
//! compile input: deleting it fails the build (measured — "Asset at
//! /assets/favicon.ico doesn't exist").
//!
//! So every tracked file an ignore pattern matches must be something no test
//! and no build step can see:
//!
//! - **not text**, because the directory walkers (`no_issue_ids.rs` over
//!   everything, `file_associations.rs` over `packaging/`) read whatever
//!   decodes as UTF-8 and skip the rest;
//! - **not named in any tracked `.rs` file**, which is how `asset!`,
//!   `include_str!`, `include_bytes!` and a test's `fs::read` reach a file.
//!
//! What this cannot see is a walker that reads raw *bytes* from a directory an
//! ignore pattern covers — none exists today. A test like that would need its
//! directory taken out of `paths-ignore`, and this file's checks extended.
//!
//! **That the Postgres suites run.** Every engine suite skips, and passes,
//! when its variable is unset. The Linux leg starts a Postgres, and
//! `HUBRO_REQUIRE_TEST_URLS` turns a lost `HUBRO_PG_TEST_URL` into a failure.
//! That is checked at run time rather than here, by
//! `ci_linux_runs_the_postgres_suites` in `tests/engine_urls.rs`: reading
//! ci.yml's text could only pin that an `echo` line exists, not that its
//! value reaches `cargo test`.

use std::process::Command;

const CI_WORKFLOW: &str = include_str!("../.github/workflows/ci.yml");

/// Every pattern under a `paths-ignore:` key, in file order. A scanner over
/// the one shape the file uses — a block list of strings — for the reason
/// `file_associations.rs` gives for its own: one property of one checked-in
/// file does not repay a YAML dependency.
///
/// Anything outside that shape panics rather than being read loosely. The
/// first version trimmed `"` off each item and nothing else, so `'docs/**'`
/// and `"docs/**" # why` both became patterns no file could match — read as
/// harmless, while GitHub honoured them and skipped CI for all of `docs/`.
fn ignored_patterns(workflow: &str) -> Vec<String> {
    let mut patterns = Vec::new();
    let mut in_list = false;
    for line in workflow.lines() {
        let code = strip_comment(line);
        let trimmed = code.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Some((key, value)) = trimmed.split_once(':') {
            if key.trim().trim_matches(|c| c == '"' || c == '\'') == "paths-ignore" {
                assert!(
                    value.trim().is_empty(),
                    "ci.yml writes paths-ignore inline ({line:?}); this scanner reads only \
                     a block list, one `- pattern` per line"
                );
                in_list = true;
                continue;
            }
        }
        if !in_list {
            continue;
        }
        match trimmed.strip_prefix('-') {
            Some(item) => patterns.push(yaml_scalar(item.trim(), line)),
            None => in_list = false,
        }
    }
    patterns
}

/// `line` up to an unquoted ` #` — YAML's comment marker needs the space (or
/// the start of the line) before it, which is what keeps `#` inside a value.
fn strip_comment(line: &str) -> &str {
    let mut quote = None;
    let mut previous = ' ';
    for (at, c) in line.char_indices() {
        match (quote, c) {
            (None, '"' | '\'') => quote = Some(c),
            (Some(open), _) if c == open => quote = None,
            (None, '#') if previous.is_whitespace() => return &line[..at],
            _ => {}
        }
        previous = c;
    }
    line
}

/// One list item's value: double-quoted, single-quoted or plain. Escapes and
/// anything after a closing quote panic — this file never needs them, and a
/// scanner that guessed would be guessing about what CI skips.
fn yaml_scalar(item: &str, line: &str) -> String {
    let unreadable = || -> ! { panic!("cannot read the paths-ignore item in {line:?}") };
    match item.chars().next() {
        Some(quote @ ('"' | '\'')) => {
            let inner = &item[1..];
            let close = inner.find(quote).unwrap_or_else(|| unreadable());
            if !inner[close + 1..].trim().is_empty() || inner[..close].contains('\\') {
                unreadable();
            }
            inner[..close].to_string()
        }
        // A plain scalar may not *start* with YAML's indicators — `*` there
        // is an alias, so `- **/*.png` unquoted is not a pattern at all.
        Some(first) if !"[]{}&*!|>%@`".contains(first) && !item.contains(['"', '\'']) => {
            item.to_string()
        }
        _ => unreadable(),
    }
}

/// Whether a GitHub filter pattern matches `path`, for the subset of the
/// syntax this workflow uses: literals, `*` (anything but `/`), and `**`
/// (anything, `**/` meaning zero or more whole directories).
///
/// Any other special character panics rather than being read as a literal: a
/// matcher that silently misread `?` or a `!` negation would answer "matches
/// nothing", and this test would pass for a pattern it never understood.
fn glob_matches(pattern: &str, path: &str) -> bool {
    if let Some(special) = pattern.chars().find(|c| "?[]!+".contains(*c)) {
        panic!("{pattern:?} uses {special:?}, which this matcher does not implement");
    }
    matches_from(pattern.as_bytes(), path.as_bytes())
}

fn matches_from(pattern: &[u8], path: &[u8]) -> bool {
    if let Some(rest) = pattern.strip_prefix(b"**/") {
        // Zero directories, or one whole directory and try again.
        return matches_from(rest, path)
            || path
                .iter()
                .position(|&b| b == b'/')
                .is_some_and(|slash| matches_from(pattern, &path[slash + 1..]));
    }
    if let Some(rest) = pattern.strip_prefix(b"**") {
        return (0..=path.len()).any(|at| matches_from(rest, &path[at..]));
    }
    if let Some(rest) = pattern.strip_prefix(b"*") {
        let segment = path.iter().position(|&b| b == b'/').unwrap_or(path.len());
        return (0..=segment).any(|at| matches_from(rest, &path[at..]));
    }
    match (pattern.split_first(), path.split_first()) {
        (None, None) => true,
        (Some((p, pattern)), Some((c, path))) if p == c => matches_from(pattern, path),
        _ => false,
    }
}

fn tracked_files() -> Vec<String> {
    let listed = Command::new("git")
        .args(["ls-files", "-z"])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .expect("git must be available to list tracked files");
    assert!(listed.status.success(), "git ls-files failed");
    listed
        .stdout
        .split(|&b| b == 0)
        .filter(|p| !p.is_empty())
        .map(|p| String::from_utf8_lossy(p).into_owned())
        .collect()
}

#[test]
fn the_matcher_reads_patterns_as_github_does() {
    // `**/` matches zero directories too — which is how `assets/**/*.ico`
    // came to cover `assets/favicon.ico`.
    assert!(glob_matches("assets/**/*.ico", "assets/favicon.ico"));
    assert!(glob_matches("assets/**/*.ico", "assets/icons/app.ico"));
    assert!(glob_matches("**/*.md", "README.md"));
    assert!(glob_matches("**/*.md", "src/db/CLAUDE.md"));
    assert!(glob_matches("docs/**", "docs/images/shot.png"));
    assert!(glob_matches("LICENSE", "LICENSE"));
    // `*` stops at a slash, and `**/` consumes whole directories only.
    assert!(!glob_matches("*.md", "docs/PERFORMANCE.md"));
    assert!(!glob_matches("assets/icons/*.ico", "assets/favicon.ico"));
    assert!(!glob_matches("a/**/b", "a/xb"));
    assert!(!glob_matches("LICENSE", "LICENSE.md"));
}

#[test]
fn the_workflow_has_no_include_filter_this_cannot_read() {
    // `paths:` (an include list) inverts the reasoning above — a file it fails
    // to list is skipped — and this file only checks `paths-ignore`. Mixing
    // the two would let the first one through unexamined.
    for line in CI_WORKFLOW.lines() {
        let key = strip_comment(line)
            .split_once(':')
            .map(|(key, _)| key.trim());
        assert_ne!(
            key,
            Some("paths"),
            "ci.yml uses an include filter; tests/ci_workflow.rs checks only paths-ignore"
        );
    }
}

#[test]
fn the_list_scanner_reads_every_way_an_item_can_be_written() {
    let workflow = "\
on:
  push:
    paths-ignore: # binary art
      - \"a/**\"
      - 'b/**'
      - c/**
      - \"d/**\" # why
      - e/x.png # why
    branches: [main]
";
    assert_eq!(
        ignored_patterns(workflow),
        ["a/**", "b/**", "c/**", "d/**", "e/x.png"]
    );
}

#[test]
#[should_panic(expected = "cannot read")]
fn the_list_scanner_refuses_what_it_cannot_read() {
    ignored_patterns("on:\n  push:\n    paths-ignore:\n      - \"a\\\"b\"\n");
}

#[test]
fn ci_ignores_no_file_a_test_or_the_build_reads() {
    let mut patterns = ignored_patterns(CI_WORKFLOW);
    // Each trigger carries its own copy; checking one is enough, and the test
    // below keeps the copies equal.
    patterns.sort();
    patterns.dedup();
    let files = tracked_files();
    // The same vacuity guard `no_issue_ids.rs` uses: a tree git cannot see
    // would match nothing and pass.
    assert!(files.len() > 100, "only {} tracked files", files.len());

    let sources: Vec<(String, String)> = files
        .iter()
        .filter(|f| f.ends_with(".rs"))
        .map(|f| {
            let text = std::fs::read_to_string(f).unwrap_or_else(|e| panic!("{f}: {e}"));
            (f.clone(), text)
        })
        .collect();

    let mut problems = Vec::new();
    for pattern in &patterns {
        // Per pattern, not in total: a pattern that matches nothing is either
        // stale or misread here, and a misread one is exactly how a real
        // ignore slips past every check below. Counting matches across the
        // whole list let one good pattern vouch for a broken one.
        let matched: Vec<&String> = files.iter().filter(|f| glob_matches(pattern, f)).collect();
        if matched.is_empty() {
            problems.push(format!(
                "{pattern:?} matches no tracked file — stale, or read wrongly by this test"
            ));
        }
        for file in matched {
            if std::fs::read_to_string(file).is_ok() {
                problems.push(format!(
                    "{pattern:?} skips CI for {file}, which is text — and \
                     tests/no_issue_ids.rs reads every tracked text file"
                ));
            }
            for (source, text) in &sources {
                if text.contains(file.as_str()) {
                    problems.push(format!(
                        "{pattern:?} skips CI for {file}, which {source} names"
                    ));
                }
            }
        }
    }
    // Not vacuous: a scanner that stopped finding the list, or a matcher that
    // stopped matching, would otherwise pass here with nothing checked. If
    // ci.yml genuinely ignores nothing one day, this file can go with it.
    assert!(
        !patterns.is_empty(),
        "no paths-ignore patterns found in ci.yml"
    );
    assert!(
        problems.is_empty(),
        "ci.yml's paths-ignore skips CI for files a test or the build \
         reads:\n{}",
        problems.join("\n")
    );
}

#[test]
fn push_and_pull_request_ignore_the_same_files() {
    // The two triggers carry separate copies of the list. They are checked
    // together above, but a copy that drifted would mean a change CI skips on
    // a pull request and runs on the merge, or the reverse.
    let push = CI_WORKFLOW
        .split_once("\n  pull_request:")
        .expect("ci.yml has a pull_request trigger");
    let (push_side, pr_side) = push;
    assert!(
        !ignored_patterns(push_side).is_empty(),
        "the push trigger's list was not found"
    );
    assert_eq!(ignored_patterns(push_side), ignored_patterns(pr_side));
}
