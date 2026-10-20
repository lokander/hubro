//! The rule in `CLAUDE.md` against Linear issue IDs in tracked files, made
//! checkable.
//!
//! The repository is public and its Linear project is not, so an issue ID (the
//! team key, a hyphen, a number) is a dead link to every outside reader — and
//! finished issues get deleted to stay under Linear's issue limit, so it
//! eventually dangles for the owner too. The habit had spread to over a thousand references before the
//! rule existed, which is why this is a test rather than a sentence.
//!
//! Scans what git tracks (`git ls-files`), so an untracked scratch file in a
//! checkout cannot fail it, and matches the ID loosely — any case, with `-`,
//! `_`, a space or nothing between the letters and the number — because test
//! names and fixture paths had carried it lowercased and underscored too.
//!
//! The matcher's own cases below are assembled with `concat!`, so this file
//! is scanned like any other instead of being exempt.

use std::process::Command;

/// Whether `text` contains a Linear issue ID in any of the forms it has been
/// written in: `fre` (any case) not preceded by a letter or digit, then
/// optionally one of `-`, `_` or a space, then a digit.
fn names_an_issue(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    let bytes = lower.as_bytes();
    lower.match_indices("fre").any(|(at, _)| {
        let starts_a_word = at == 0 || !bytes[at - 1].is_ascii_alphanumeric();
        let mut next = at + 3;
        if matches!(bytes.get(next), Some(b'-' | b'_' | b' ')) {
            next += 1;
        }
        starts_a_word && bytes.get(next).is_some_and(u8::is_ascii_digit)
    })
}

#[test]
fn the_matcher_finds_every_form_an_id_has_been_written_in() {
    for named in [
        concat!("FR", "E-162"),
        concat!("(see FR", "E-1)."),
        concat!("a_pre_f", "re_120_config"),
        concat!("connections_pre_f", "re_120.toml"),
        concat!("localhost/f", "re45-real-store"),
        concat!("Fr", "e 7"),
    ] {
        assert!(names_an_issue(named), "{named}");
    }
    for clean in [
        "free 22",
        "offre-12",
        "fresh_3",
        "lokander/fre-<number>-short-issue-title",
        "Work is tracked in Linear (team FRE).",
    ] {
        assert!(!names_an_issue(clean), "{clean}");
    }
}

#[test]
fn no_tracked_file_names_a_linear_issue() {
    let listed = Command::new("git")
        .args(["ls-files", "-z"])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .expect("git must be available to list tracked files");
    assert!(listed.status.success(), "git ls-files failed");
    let mut offenders = Vec::new();
    for path in listed.stdout.split(|&b| b == 0).filter(|p| !p.is_empty()) {
        let path = String::from_utf8_lossy(path).into_owned();
        let full = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(&path);
        // Binary assets (icons, fonts) are not prose; skip what is not text.
        let Ok(text) = std::fs::read_to_string(&full) else {
            continue;
        };
        for (number, line) in text.lines().enumerate() {
            if names_an_issue(line) {
                offenders.push(format!("{path}:{}: {}", number + 1, line.trim()));
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "tracked files name Linear issues; state the reason instead \
         (CLAUDE.md, Issue workflow):\n{}",
        offenders.join("\n")
    );
}
