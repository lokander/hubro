//! What `ci.yml` declines to run on, checked against what the build and the
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

use std::process::Command;

const CI_WORKFLOW: &str = include_str!("../.github/workflows/ci.yml");

/// Every pattern under a `paths-ignore:` key, in file order. A scanner over
/// the one shape the file uses — a block list of quoted strings — for the
/// reason `file_associations.rs` gives for its own: one property of one
/// checked-in file does not repay a YAML dependency.
fn ignored_patterns(workflow: &str) -> Vec<String> {
    let mut patterns = Vec::new();
    let mut in_list = false;
    for line in workflow.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('#') || trimmed.is_empty() {
            continue;
        }
        if trimmed == "paths-ignore:" {
            in_list = true;
            continue;
        }
        if !in_list {
            continue;
        }
        match trimmed.strip_prefix("- ") {
            Some(item) => patterns.push(item.trim().trim_matches('"').to_string()),
            None => in_list = false,
        }
    }
    patterns
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
    assert!(glob_matches("assets/**/*.ico", "assets/icons/icon.ico"));
    assert!(glob_matches("**/*.md", "README.md"));
    assert!(glob_matches("**/*.md", "src/db/CLAUDE.md"));
    assert!(glob_matches("docs/**", "docs/screenshot.png"));
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
        assert_ne!(
            line.trim(),
            "paths:",
            "ci.yml uses an include filter; tests/ci_triggers.rs checks only paths-ignore"
        );
    }
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

    let mut matched = 0;
    let mut problems = Vec::new();
    for pattern in &patterns {
        for file in files.iter().filter(|f| glob_matches(pattern, f)) {
            matched += 1;
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
    assert!(matched > 0, "no tracked file matches {patterns:?}");
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
