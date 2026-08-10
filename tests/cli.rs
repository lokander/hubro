//! The command line's observable contract, checked by running the real binary
//! (FRE-114).
//!
//! `cli`'s unit tests cover the parsing; this covers what `main` does with the
//! result, which is the part a user sees and the part no unit test reaches:
//! which stream each answer goes to, which exit status it carries, and — the
//! one that matters most for a GUI app — that these invocations answer and
//! stop instead of opening a window.
//!
//! Every case here fails or exits before `dioxus::launch`, so no window is
//! ever created and the suite stays runnable headless.

use std::process::{Command, Output};

fn hubro(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_hubro"))
        .args(args)
        .output()
        .expect("running hubro")
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[test]
fn help_goes_to_stdout_and_succeeds() {
    let output = hubro(&["--help"]);
    assert!(output.status.success(), "{}", stderr(&output));
    let text = stdout(&output);
    // Usage output belongs on stdout so `hubro --help | less` works.
    assert!(text.contains("Usage:"), "{text}");
    assert!(text.contains("--version"), "{text}");
    assert!(stderr(&output).is_empty());
    assert_eq!(stdout(&hubro(&["-h"])), text, "-h and --help must agree");
}

#[test]
fn version_prints_the_crate_version() {
    let output = hubro(&["--version"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(
        stdout(&output).trim(),
        format!("hubro {}", env!("CARGO_PKG_VERSION"))
    );
    assert_eq!(stdout(&hubro(&["-V"])), stdout(&output));
}

#[test]
fn an_unknown_option_fails_with_a_usage_status() {
    let output = hubro(&["--frobnicate"]);
    assert_eq!(output.status.code(), Some(2), "{}", stderr(&output));
    let text = stderr(&output);
    assert!(text.contains("--frobnicate"), "{text}");
    // A usage mistake has an answer in the help; the message points at it.
    assert!(text.contains("--help"), "{text}");
    assert!(stdout(&output).is_empty(), "errors belong on stderr");
}

#[test]
fn two_databases_are_refused_rather_than_silently_ignored() {
    let output = hubro(&["one.db", "two.db"]);
    assert_eq!(output.status.code(), Some(2), "{}", stderr(&output));
    assert!(stderr(&output).contains("one database per window"));
}

#[test]
fn a_file_that_is_not_a_database_is_refused_before_a_window_opens() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("holiday-photo.db");
    std::fs::write(&path, b"\xff\xd8\xff\xe0 JFIF, definitely not a database").unwrap();
    let output = hubro(&[path.to_str().unwrap()]);
    // A failure, not a usage mistake: the command was well-formed.
    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    let text = stderr(&output);
    assert!(text.contains("not a SQLite database"), "{text}");
    assert!(text.contains("holiday-photo.db"), "{text}");
    // The driver's own wording is what this whole path exists to replace.
    assert!(!text.contains("code: 14"), "{text}");
    // …and no "Try --help", which would be no help at all here.
    assert!(!text.contains("--help"), "{text}");
}

#[test]
fn a_missing_file_names_the_path_that_is_missing() {
    let output = hubro(&["/nonexistent/directory/app.sqlite"]);
    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    assert!(
        stderr(&output).contains("no such file: /nonexistent/directory/app.sqlite"),
        "{}",
        stderr(&output)
    );
}

#[test]
fn an_unsupported_url_scheme_says_which_schemes_work() {
    let output = hubro(&["mysql://user@db.example.com/app"]);
    assert_eq!(output.status.code(), Some(2), "{}", stderr(&output));
    let text = stderr(&output);
    assert!(text.contains("mysql://"), "{text}");
    assert!(text.contains("postgres://"), "{text}");
    assert!(text.contains("mssql://"), "{text}");
}
