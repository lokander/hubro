//! Holds the command line to its promise that a password never gets copied
//! anywhere (FRE-114).
//!
//! A connection URL may carry a password, and one typed on a command line is
//! already exposed — it is in the shell's history file and readable from the
//! process list. hubro cannot undo that, and `--help` says so. What hubro can
//! do is not make it worse: not echo it into an error, not write it into a tab
//! title or a saved connection name, not leave it in the locator that goes to
//! `connections.toml` and keys the keyring.
//!
//! That promise is spread over half a dozen small decisions in `cli`, each of
//! which is individually easy to get right and collectively easy to break by
//! adding one `format!`. So instead of testing the decisions, this pushes one
//! password through every path that can render user input and asserts the
//! password is not in the output — including through the real binary, which is
//! the only check that covers the wiring in `main` as well as the library.

use std::ffi::OsString;
use std::process::Command;

use hubro::cli::{classify, display_name, parse, redact_url};

/// Distinctive enough that a substring search cannot match it by accident.
const SECRET: &str = "hunter2-Sw0rdf1sh";

/// Every command line that carries the password and is *rejected*, so its text
/// reaches an error message.
fn rejected_lines() -> Vec<Vec<String>> {
    vec![
        // A scheme with no backend: the message names the scheme.
        vec![format!("mysql://user:{SECRET}@db.example.com/app")],
        // A supported scheme that fails validation (port 0).
        vec![format!("postgres://user:{SECRET}@db.example.com:0/app")],
        vec![format!("mssql://sa:{SECRET}@db.example.com:0/app")],
        // Not a URL at all, but still carrying one's shape.
        vec![format!("postgres://user:{SECRET}@")],
        // An option that isn't ours, whose value is the URL.
        vec![format!("--connect=postgres://user:{SECRET}@host/app")],
        // The same, with the separators that a URL uses structurally but that
        // are ordinary characters *inside a password*. Each of these defeated
        // the first redactor, which bounded the userinfo at the first `/`, `?`
        // or `#` after the scheme and so stopped looking before the `@`; the
        // whole argument was then echoed verbatim by the unknown-option error.
        // `--dbname=<url>` is the shape a psql habit produces, and a password
        // with a slash in it is not unusual.
        vec![format!("--connect=postgres://user:{SECRET}/x@host/app")],
        vec![format!("--dbname=postgres://user:{SECRET}?q@host/app")],
        vec![format!("--url=postgres://user:{SECRET}#f@host/app")],
        // …and as a positional, where the URL validation reports the failure.
        vec![format!("postgres://user:{SECRET}/x@host/app")],
        // An option that is itself URL-shaped, so there is no `=` to truncate
        // at and redaction is the only thing standing between it and stderr.
        vec![format!("--postgres://user:{SECRET}@host/app")],
        // Two positionals: the second is never even looked at.
        vec![
            "app.db".to_string(),
            format!("postgres://user:{SECRET}@host/app"),
        ],
    ]
}

fn assert_secret_free(text: &str, what: &str) {
    assert!(
        !text.contains(SECRET),
        "the password leaked into {what}: {text}"
    );
}

#[test]
fn no_rejected_command_line_echoes_its_password() {
    for line in rejected_lines() {
        let args: Vec<OsString> = line.iter().map(OsString::from).collect();
        let err = parse(args)
            .err()
            .unwrap_or_else(|| panic!("{line:?} should have been rejected"));
        assert_secret_free(&err.to_string(), "a CLI error's Display");
        // `Debug` is the easier accident — a `{:?}` in a log line, a panic
        // message, an `unwrap` on a Result. It is implemented as Display for
        // exactly this reason.
        assert_secret_free(&format!("{err:?}"), "a CLI error's Debug");
    }
}

#[test]
fn an_accepted_url_keeps_its_password_out_of_everything_persisted() {
    let raw = format!("postgres://user:{SECRET}@db.example.com:5432/app");
    let target = classify(std::ffi::OsStr::new(&raw)).unwrap();

    // The locator is written to connections.toml and used as the keyring
    // account key; it is also what these two render.
    assert_secret_free(&target.to_string(), "the target's Display");
    assert_secret_free(&format!("{target:?}"), "the target's Debug");

    let hubro::cli::OpenTarget::Server { url, password, .. } = &target else {
        panic!("a postgres:// URL must classify as a server target");
    };
    assert_secret_free(url, "the normalized locator");
    // …and the password is still available, or the URL would simply not work.
    assert_eq!(password.as_deref(), Some(SECRET));

    // The name is what a saved entry and the tab bar show.
    assert_secret_free(&display_name(url), "the display name");
    assert_secret_free(&display_name(&raw), "the display name built from a raw URL");
}

#[test]
fn redaction_survives_text_that_is_not_a_valid_url() {
    // The strings most in need of redaction are the ones that failed to parse,
    // which is why the redactor works on raw text rather than on a parsed URL.
    for text in [
        format!("postgres://user:{SECRET}@ho st/app"),
        format!("postgres://user:{SECRET}@"),
        format!("invalid URL: postgres://u:{SECRET}@h:99999/db"),
        format!("mssql://user:{SECRET}@[not-an-ipv6/app"),
        format!("connecting to postgres://u:{SECRET}@h/db failed, and then"),
        // The separators a URL gives structural meaning and a password does
        // not: nothing about them may end the search for the `@`.
        format!("postgres://user:{SECRET}/x@host/app"),
        format!("postgres://user:{SECRET}?q@host/app"),
        format!("postgres://user:{SECRET}#f@host/app"),
        format!("postgres://user:a/b?c#d{SECRET}@host/app"),
        // Two URLs in one message: redacting the first must not stop the scan.
        format!("tried postgres://a:{SECRET}@h/db then mssql://b:{SECRET}@h2/db"),
    ] {
        assert_secret_free(&redact_url(&text), "redacted text");
    }
}

#[test]
fn redaction_leaves_ordinary_text_and_password_free_urls_alone() {
    // The counterweight to the test above: a redactor that mangled everything
    // would pass it. Over-reaching is the safe direction, but not into text
    // that has no credential in it at all.
    for text in [
        "postgres://user@host:5432/app",
        "mssql://host/app",
        "no such file: /var/lib/app.db",
        "unknown option `--frobnicate`",
        "postgres://user@h/db and mail admin@example.com",
        "",
        "://",
    ] {
        assert_eq!(redact_url(text), text, "{text}");
    }
    // A password is replaced, and nothing around it is.
    assert_eq!(
        redact_url("postgres://user:hunter2/x@host:5432/app?sslmode=require"),
        "postgres://user:***@host:5432/app?sslmode=require"
    );
}

#[test]
fn the_binary_never_prints_a_password_it_was_given() {
    // The end-to-end version of the claim: everything above tests the library,
    // and this tests what a user would actually see. These invocations all
    // fail before the window opens, so no app is launched.
    for line in rejected_lines() {
        let output = Command::new(env!("CARGO_BIN_EXE_hubro"))
            .args(&line)
            .output()
            .expect("running hubro");
        assert!(!output.status.success(), "{line:?} should have failed");
        assert_secret_free(
            &String::from_utf8_lossy(&output.stderr),
            "the binary's stderr",
        );
        assert_secret_free(
            &String::from_utf8_lossy(&output.stdout),
            "the binary's stdout",
        );
    }
}

#[test]
fn the_help_text_warns_about_passwords_on_a_command_line() {
    // The one thing hubro cannot fix by redacting: the password is in the
    // shell's history and in `ps` before hubro ever sees it. Saying so is the
    // whole mitigation, so it must not quietly disappear from the help.
    let help = hubro::cli::HELP.to_lowercase();
    assert!(help.contains("password"), "{}", hubro::cli::HELP);
    assert!(help.contains("history"), "{}", hubro::cli::HELP);
    assert!(
        help.contains("keyring"),
        "the help should point at the alternative, not just the risk"
    );
}
