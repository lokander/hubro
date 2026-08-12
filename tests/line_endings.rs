//! The `.gitattributes` LF rule, made checkable (FRE-160).
//!
//! Several tests read a checked-in file and compare it byte for byte, or match
//! it against a literal containing `\n`. That only works while git delivers the
//! file unconverted — and git's default on Windows (`core.autocrlf=true`) does
//! convert, which is what turned two merged PRs into three red Windows builds.
//! `.gitattributes` pins the line endings; this names the files that depend on
//! it, so deleting the rule fails here rather than somewhere less obvious.
//!
//! **For the `eol=lf` files this can only fire where git converts**, i.e. on a
//! Windows checkout: on Linux and macOS they pass whether or not
//! `.gitattributes` exists, so there it is a diagnosis rather than a gate,
//! turning "an untouched file must be rewritten unchanged" into "this file
//! arrived with CRLF". The gate for those is the Windows leg of CI, which runs
//! only on push to `main` or a dispatch.
//!
//! **For anything under `tests/fixtures/` it is a gate everywhere**, because
//! `-text` is symmetric: a fixture authored on Windows commits its CRLF bytes
//! and keeps them on every platform, so a byte-exact fixture can go wrong with
//! no Windows checkout involved — and this is what catches it.

/// Every repo file read by a test that is sensitive to line endings, with the
/// test that depends on it. Adding a byte-exact test over a new file means
/// adding it here.
///
/// `README.md` is deliberately absent even though `.gitattributes` names it:
/// `tests/support_matrix.rs` reads it through `str::lines()`, which strips a
/// trailing `\r`, so it does not care. Same for the three modules that read
/// their own source — `ui/grid/stats.rs` counts markers, `connect.rs`'s
/// `method_body` matches braces, and `connections.rs` slices its prompt card
/// by lines; none compares a `\n`.
///
/// That last one is a rule the source tests have to keep choosing (FRE-162):
/// the natural way to pin a line of code is a literal like `"\n    field,\n"`,
/// which reads as precise and fails on a CRLF checkout for a reason that has
/// nothing to do with the code under test. `lines()` plus `trim` says the same
/// thing and costs nothing.
const LF_ONLY: &[(&str, &str)] = &[
    (
        "tests/fixtures/connections_pre_fre_120.toml",
        "config_back_compat.rs compares the round trip byte for byte",
    ),
    (
        "packaging/macos/Info.plist",
        r#"file_associations.rs matches "</key>\n\t<true/>""#,
    ),
    (
        "packaging/linux/hubro.desktop.hbs",
        "file_associations.rs mirrors dx's generated entry",
    ),
    (
        "packaging/windows/file-associations.wxs",
        "file_associations.rs reads it with include_str!",
    ),
    ("Cargo.toml", "several tests re-derive values from it"),
    ("Dioxus.toml", "several tests re-derive values from it"),
    (
        ".github/workflows/release.yml",
        r#"file_associations.rs splits it on "\njobs:\n" to read the job graph"#,
    ),
];

#[test]
fn the_files_tests_read_byte_for_byte_arrive_with_unix_line_endings() {
    for (path, why) in LF_ONLY {
        let bytes = std::fs::read(path).unwrap_or_else(|e| panic!("{path}: {e}"));
        let carriage_returns = bytes.iter().filter(|b| **b == b'\r').count();
        assert_eq!(
            carriage_returns, 0,
            "{path} arrived with {carriage_returns} CR bytes, so git converted it \
             on checkout — {why}, which compares against LF. Check .gitattributes \
             still covers this path, and re-clone or run `git add --renormalize .` \
             to repair a working copy checked out before the rule existed."
        );
    }
}
