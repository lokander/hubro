//! Command-line arguments and OS "open this" requests (FRE-114).
//!
//! `hubro <file>` opens a SQLite file and `hubro <url>` opens a server
//! connection, which is also how a file manager or the shell hands hubro a
//! database: on Linux and Windows the file association passes the path in
//! `argv`, on macOS it arrives after launch as an `open` Apple Event (see
//! [`deliver_opened_url`]). Both routes end at the same [`OpenTarget`], so the
//! app has one way to be told what to open.
//!
//! Parsing is deliberately hand-written — one positional argument plus
//! `--help`/`--version` is not worth a CLI framework, and a dependency here
//! would be a dependency in the shipped binary.
//!
//! **Everything in this module treats its input as secret-bearing.** A URL on
//! a command line can carry a password (which is already visible in shell
//! history and in `ps` output — [`HELP`] says so), and hubro must not make
//! that worse by copying it anywhere it persists. So:
//!
//! - [`CliError`] renders every echoed fragment through [`redact_url`], and
//!   its `Debug` is its `Display` so a `{:?}` cannot leak past it;
//!   [`OpenTarget`] does the same and never formats its password at all.
//! - The password is split off the URL at classification time. What reaches
//!   the connect flow is the normalized, password-free locator — the same
//!   string the saved-connections list and the keyring key are built from —
//!   with the password carried beside it, put into session memory only.
//! - [`display_name`] builds a tab/entry name from the URL's host and database
//!   components rather than from the URL text, so the name that *is* persisted
//!   cannot contain a credential.
//!
//! `tests/cli_secrets.rs` holds those claims to a password-bearing URL pushed
//! through every one of these paths.

use std::ffi::{OsStr, OsString};
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};

use crate::config::BackendKind;
use crate::db::{
    check_sqlite_file, normalize_mssql_url, normalize_pg_url, DbError, SqliteFileError,
};

/// The file extensions hubro registers itself as a handler for.
///
/// The single source of truth for the packaging declarations: the `.desktop`
/// MIME globs, the macOS `CFBundleDocumentTypes`, and the Windows registry
/// fragment all list exactly these, and `tests/file_associations.rs` fails if
/// one of them drifts. `db` is deliberately included even though it is a
/// contested extension — hubro registers as *a* handler for it, never as the
/// default (see `packaging/README.md`).
pub const DATABASE_EXTENSIONS: [&str; 3] = ["db", "sqlite", "sqlite3"];

/// hubro's version, as reported by `--version`.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// `--help` output. Ends with the credential warning: a password typed on a
/// command line is already in the shell's history file and readable from the
/// process list, and the only fix is not to type it.
pub const HELP: &str = "\
hubro — desktop viewer for SQLite, Postgres, and SQL Server databases

Usage:
  hubro [<file> | <url>]
  hubro --help
  hubro --version

Arguments:
  <file>  Path to a SQLite database file (.db, .sqlite, .sqlite3). Opened in a
          tab straight away; it is not added to the saved connections list.
  <url>   A connection URL: postgres://, postgresql://, mssql:// or
          sqlserver://. Use `--` before an argument that starts with `-`.

With no argument hubro opens the connections screen and restores the tabs from
the previous session.

A password given in a URL is visible in your shell history and to anyone who
can list processes. Leave it out — hubro will ask for it, and can remember it
in the system keyring instead.
";

/// What a command line asked hubro to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Invocation {
    /// Launch the app, opening `.0` first when there is one.
    Run(Option<OpenTarget>),
    /// Print [`HELP`] and exit; no window opens.
    Help,
    /// Print [`VERSION`] and exit; no window opens.
    Version,
}

/// A database hubro was asked to open, from `argv` or from the OS.
///
/// No `Debug` derive: the server variant holds a password. The manual impl
/// below prints the same redacted form as [`Display`](std::fmt::Display), so
/// a stray `{:?}` in a log or a panic message cannot spill it.
#[derive(Clone, PartialEq, Eq)]
pub enum OpenTarget {
    /// A SQLite database file.
    File(PathBuf),
    /// A Postgres or SQL Server connection.
    Server {
        backend: BackendKind,
        /// The **normalized, password-free** locator (`normalize_pg_url` /
        /// `normalize_mssql_url`) — identical to what a connection saved from
        /// the form would carry, so an argv connect dedupes against a saved
        /// entry and reuses its keyring secret.
        url: String,
        /// The password lifted out of the URL, percent-decoded, or `None` when
        /// the URL carried none (the ordinary case, and the one the help text
        /// asks for). Only ever placed in session memory.
        password: Option<String>,
    },
}

impl OpenTarget {
    /// Checks what can be known without touching the network, so an
    /// unopenable file is reported on the terminal instead of opening a
    /// window whose only content is an error.
    ///
    /// Only the file case has such an answer: whether a path exists and starts
    /// with a SQLite header is a local fact, while whether a server accepts a
    /// connection is not — a URL therefore always launches the app, which is
    /// where a refused connection, a password prompt or an SSH host-key
    /// question belongs.
    pub fn preflight(&self) -> Result<(), CliError> {
        match self {
            OpenTarget::File(path) => check_sqlite_file(path).map_err(CliError::File),
            OpenTarget::Server { .. } => Ok(()),
        }
    }
}

impl std::fmt::Display for OpenTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OpenTarget::File(path) => write!(f, "{}", redact_url(&path.display().to_string())),
            // `url` is the normalized locator and so already password-free;
            // redacting again costs nothing and means this holds even if the
            // field is ever set from somewhere else.
            OpenTarget::Server { url, .. } => write!(f, "{}", redact_url(url)),
        }
    }
}

impl std::fmt::Debug for OpenTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self}")
    }
}

/// A command line hubro can't act on.
///
/// Variants rather than a pre-rendered string so the wording lives in one
/// `Display`, and so nothing can construct an error whose text bypasses
/// [`redact_url`].
#[derive(Clone, PartialEq, Eq)]
pub enum CliError {
    /// An option hubro doesn't have.
    UnknownOption(String),
    /// A second positional argument. hubro opens one database per window.
    TooManyArguments,
    /// A URL scheme naming an engine hubro has no backend for. Holds the
    /// scheme alone, which is the part worth naming and the part that cannot
    /// contain a credential.
    UnsupportedScheme(String),
    /// A URL of a supported scheme that isn't usable (bad syntax, port 0, …).
    /// The message comes from the shared URL validation, which names the
    /// problem and never quotes the URL.
    UnusableUrl(String),
    /// The path isn't a SQLite database hubro can open.
    File(SqliteFileError),
}

impl CliError {
    /// Process exit status. `2` is the conventional "you invoked me wrong"
    /// code — a usage problem, fixable by retyping the command; `1` is an
    /// ordinary failure to do what was asked.
    pub fn exit_code(&self) -> i32 {
        match self {
            CliError::UnknownOption(_)
            | CliError::TooManyArguments
            | CliError::UnsupportedScheme(_)
            | CliError::UnusableUrl(_) => 2,
            CliError::File(_) => 1,
        }
    }

    /// Whether `--help` is worth pointing at. A usage mistake has an answer
    /// there; a file that isn't a database does not.
    pub fn is_usage(&self) -> bool {
        self.exit_code() == 2
    }
}

impl std::fmt::Display for CliError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CliError::UnknownOption(flag) => {
                write!(f, "unknown option `{}`", redact_url(flag))
            }
            CliError::TooManyArguments => write!(
                f,
                "expected at most one file or URL; hubro opens one database per window"
            ),
            CliError::UnsupportedScheme(scheme) => write!(
                f,
                "unsupported URL scheme `{}://` — hubro opens postgres:// and mssql:// URLs, \
                 or the path to a SQLite database file",
                redact_url(scheme)
            ),
            CliError::UnusableUrl(message) => write!(f, "{}", redact_url(message)),
            CliError::File(err) => write!(f, "{}", redact_url(&err.to_string())),
        }
    }
}

impl std::fmt::Debug for CliError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self}")
    }
}

impl std::error::Error for CliError {}

/// Replaces the password in any `scheme://user:password@host` text with `***`.
///
/// Works on the raw string rather than on a parsed [`url::Url`], because the
/// strings that most need redacting are the ones that failed to parse — an
/// error message about a malformed URL must not quote the malformed URL back
/// with its password intact. Text with no `://`, or with no `user:pw@`
/// authority, is returned unchanged, so this is safe to apply to any message.
pub fn redact_url(text: &str) -> String {
    let Some(scheme_end) = text.find("://") else {
        return text.to_string();
    };
    let authority_start = scheme_end + 3;
    // The authority runs to the first path/query/fragment separator.
    let authority_end = text[authority_start..]
        .find(['/', '?', '#'])
        .map(|i| authority_start + i)
        .unwrap_or(text.len());
    let authority = &text[authority_start..authority_end];
    // Userinfo ends at the *last* `@` in the authority: `@` is legal inside a
    // percent-encoded password, and taking the first one would leave the tail
    // of the secret in place.
    let Some(at) = authority.rfind('@') else {
        return text.to_string();
    };
    let userinfo = &authority[..at];
    let Some(colon) = userinfo.find(':') else {
        return text.to_string(); // a user with no password
    };
    format!(
        "{}{}:***{}",
        &text[..authority_start],
        &userinfo[..colon],
        &text[authority_start + at..]
    )
}

/// Parses hubro's arguments (the caller passes `args_os().skip(1)`).
///
/// Pure: it never touches the filesystem, so every case is a unit test. The
/// one check that needs the disk is [`OpenTarget::preflight`], which `main`
/// runs after.
pub fn parse<I>(args: I) -> Result<Invocation, CliError>
where
    I: IntoIterator<Item = OsString>,
{
    let mut positional: Option<OsString> = None;
    let mut options_done = false;
    for arg in args {
        if !options_done {
            match arg.to_str() {
                // Everything after `--` is the positional, so a file whose
                // name starts with `-` is still reachable.
                Some("--") => {
                    options_done = true;
                    continue;
                }
                Some("-h") | Some("--help") => return Ok(Invocation::Help),
                Some("-V") | Some("--version") => return Ok(Invocation::Version),
                Some(flag) if flag.starts_with('-') => {
                    return Err(CliError::UnknownOption(flag.to_string()))
                }
                // Not an option, or not valid UTF-8 (so it can't be one of
                // ours, and a path must survive byte-for-byte).
                _ => {}
            }
        }
        if positional.is_some() {
            return Err(CliError::TooManyArguments);
        }
        positional = Some(arg);
    }
    match positional {
        Some(arg) => Ok(Invocation::Run(Some(classify(&arg)?))),
        None => Ok(Invocation::Run(None)),
    }
}

/// Decides whether an argument names a file or a server, and validates it.
///
/// The discriminator is a `scheme://` prefix, not a guess about the text: a
/// Windows path (`C:\db\app.db`) and a UNC path have no `://`, and a POSIX
/// path can't have one before its first `/`. So anything without it is a path,
/// and anything with it must be a scheme hubro knows — which makes `mysql://`
/// a named error instead of a baffling "no such file".
pub fn classify(arg: &OsStr) -> Result<OpenTarget, CliError> {
    let Some(text) = arg.to_str() else {
        // Not UTF-8: it cannot be a URL, and PathBuf keeps the bytes intact.
        return Ok(OpenTarget::File(PathBuf::from(arg)));
    };
    let Some(scheme) = url_scheme(text) else {
        return Ok(OpenTarget::File(PathBuf::from(text)));
    };
    match scheme.to_ascii_lowercase().as_str() {
        "postgres" | "postgresql" => server_target(BackendKind::Postgres, text),
        "mssql" | "sqlserver" => server_target(BackendKind::SqlServer, text),
        // macOS hands over opened files as `file://` URLs, and a `.desktop`
        // entry launched with `%u` does too.
        "file" => url::Url::parse(text)
            .ok()
            .and_then(|url| url.to_file_path().ok())
            .map(OpenTarget::File)
            .ok_or_else(|| CliError::UnusableUrl("not a local file URL".to_string())),
        other => Err(CliError::UnsupportedScheme(other.to_string())),
    }
}

/// The scheme of `text` when it is written as `scheme://…`, per RFC 3986's
/// scheme grammar (letter first, then letters/digits/`+`/`-`/`.`).
fn url_scheme(text: &str) -> Option<&str> {
    let scheme = &text[..text.find("://")?];
    let mut chars = scheme.chars();
    if !chars.next()?.is_ascii_alphabetic() {
        return None;
    }
    chars
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
        .then_some(scheme)
}

/// Splits a server URL into the password-free locator the app connects and
/// saves under, plus the password to remember for this session only.
fn server_target(backend: BackendKind, raw: &str) -> Result<OpenTarget, CliError> {
    // `normalize_*` validates the scheme, port and host, and strips the
    // password — the same canonical form the connections list stores, so an
    // argv connect lands on an existing saved entry rather than beside it.
    let url = match backend {
        BackendKind::SqlServer => normalize_mssql_url(raw),
        _ => normalize_pg_url(raw),
    }
    .map_err(|err| match err {
        // Unwrapped: `DbError::Connect` displays as "connection failed: …",
        // which is a lie here — nothing has been connected yet.
        DbError::Connect(message) => CliError::UnusableUrl(message),
        other => CliError::UnusableUrl(other.to_string()),
    })?;
    // Parsed from the raw URL because normalization has already removed it.
    // The url crate percent-encodes on parse; the connect flow re-encodes when
    // it splices the password back in, so it must be handed the decoded form.
    let password = url::Url::parse(raw)
        .ok()
        .and_then(|parsed| {
            parsed.password().map(|pw| {
                percent_encoding::percent_decode_str(pw)
                    .decode_utf8()
                    .map(|s| s.into_owned())
                    .unwrap_or_else(|_| pw.to_string())
            })
        })
        .filter(|pw| !pw.is_empty());
    Ok(OpenTarget::Server {
        backend,
        url,
        password,
    })
}

/// The tab and saved-entry name for a server URL opened from the command
/// line: `database@host`, or the host alone when the URL names no database.
///
/// Built from the parsed components rather than from the URL text, which is
/// what keeps a credential out of a name that gets written to
/// `connections.toml` — a redaction that is impossible to forget rather than
/// one that must be remembered.
pub fn display_name(url: &str) -> String {
    let Ok(parsed) = url::Url::parse(url) else {
        return "database".to_string();
    };
    let host = parsed.host_str().unwrap_or("database");
    match parsed.path().trim_start_matches('/') {
        "" => host.to_string(),
        database => format!("{database}@{host}"),
    }
}

/// Targets the OS handed to an already-launched app: on macOS a double-clicked
/// file arrives as an `open` Apple Event (tao's `Event::Opened`) rather than in
/// `argv`, so the association cannot work without this path.
///
/// A process-global channel rather than a signal, because the sender is the
/// event loop — outside any Dioxus scope — and the receiver is a task in the
/// UI. Unbounded and created on first use, so an event that arrives before the
/// UI is up is queued rather than dropped: at launch-by-double-click the Apple
/// Event beats the first render.
struct OpenedChannel {
    tx: UnboundedSender<OpenTarget>,
    rx: Mutex<Option<UnboundedReceiver<OpenTarget>>>,
}

static OPENED: OnceLock<OpenedChannel> = OnceLock::new();

fn opened_channel() -> &'static OpenedChannel {
    OPENED.get_or_init(|| {
        let (tx, rx) = unbounded_channel();
        OpenedChannel {
            tx,
            rx: Mutex::new(Some(rx)),
        }
    })
}

/// Queues a URL the OS asked hubro to open. Called from the event loop, so it
/// never blocks and never panics: a URL that doesn't classify (an unsupported
/// scheme, a remote `file://`) is dropped, because there is no window
/// guaranteed to exist yet to report it in.
pub fn deliver_opened_url(url: &url::Url) {
    if let Ok(target) = classify(OsStr::new(url.as_str())) {
        let _ = opened_channel().tx.send(target);
    }
}

/// Takes the receiving end of that queue. Returns `Some` exactly once — the
/// UI claims it at startup, and a second caller would silently steal the
/// deliveries from the first.
pub fn take_opened() -> Option<UnboundedReceiver<OpenTarget>> {
    opened_channel().rx.lock().ok()?.take()
}

/// The startup target, handed to the app as a launch context so the UI can
/// open it once the state exists. `None` for a plain `hubro`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Startup(pub Option<OpenTarget>);

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<OsString> {
        list.iter().map(OsString::from).collect()
    }

    #[test]
    fn help_and_version_win_over_everything_else() {
        // Both are answered before any target is looked at, so `--help` works
        // even when the rest of the line is nonsense.
        assert_eq!(parse(args(&["--help"])).unwrap(), Invocation::Help);
        assert_eq!(parse(args(&["-h"])).unwrap(), Invocation::Help);
        assert_eq!(parse(args(&["--version"])).unwrap(), Invocation::Version);
        assert_eq!(parse(args(&["-V"])).unwrap(), Invocation::Version);
        assert_eq!(
            parse(args(&["a.db", "--help"])).unwrap(),
            Invocation::Help,
            "help must not be blocked by a bad argument before it"
        );
    }

    #[test]
    fn no_argument_just_launches() {
        assert_eq!(parse(args(&[])).unwrap(), Invocation::Run(None));
    }

    #[test]
    fn a_path_is_a_file_target() {
        for path in [
            "app.db",
            "./app.sqlite",
            "/var/lib/app.sqlite3",
            "relative/dir/db",
            // Windows: `C:` is followed by a separator, never `://`.
            r"C:\Users\me\app.db",
            r"\\server\share\app.db",
        ] {
            assert_eq!(
                parse(args(&[path])).unwrap(),
                Invocation::Run(Some(OpenTarget::File(PathBuf::from(path)))),
                "{path}"
            );
        }
    }

    #[test]
    fn a_supported_url_is_a_server_target() {
        let target = classify(OsStr::new("postgres://user@db.example.com/app")).unwrap();
        assert_eq!(
            target,
            OpenTarget::Server {
                backend: BackendKind::Postgres,
                // Normalized: the default port is filled in, exactly as a
                // saved entry would be.
                url: "postgres://user@db.example.com:5432/app".to_string(),
                password: None,
            }
        );
        let target = classify(OsStr::new("sqlserver://sa@DB.example.com/app")).unwrap();
        assert_eq!(
            target,
            OpenTarget::Server {
                backend: BackendKind::SqlServer,
                // Alias scheme canonicalized, host lowercased.
                url: "mssql://sa@db.example.com:1433/app".to_string(),
                password: None,
            }
        );
    }

    #[test]
    fn a_url_password_is_split_off_the_locator() {
        // The locator is what gets saved and what keys the keyring, so the
        // password must not be in it; the password itself is returned decoded,
        // because the connect flow re-encodes when it splices it back.
        let target = classify(OsStr::new("postgres://user:p%40ss@host:5432/app")).unwrap();
        assert_eq!(
            target,
            OpenTarget::Server {
                backend: BackendKind::Postgres,
                url: "postgres://user@host:5432/app".to_string(),
                password: Some("p@ss".to_string()),
            }
        );
        // An empty password (`user:@host`) is no password at all.
        let target = classify(OsStr::new("postgres://user:@host:5432/app")).unwrap();
        assert!(matches!(target, OpenTarget::Server { password: None, .. }));
    }

    #[test]
    fn an_unknown_scheme_is_named_rather_than_treated_as_a_path() {
        let err = classify(OsStr::new("mysql://user@host/app")).unwrap_err();
        assert!(err.to_string().contains("mysql://"), "{err}");
        assert!(err.to_string().contains("postgres://"), "{err}");
        // `sqlite://` too: the answer is a plain path, and the message says so.
        let err = classify(OsStr::new("sqlite://app.db")).unwrap_err();
        assert!(err.to_string().contains("sqlite://"), "{err}");
        assert!(err.to_string().contains("file"), "{err}");
    }

    #[test]
    fn a_file_url_resolves_to_its_path() {
        assert_eq!(
            classify(OsStr::new("file:///var/lib/app.db")).unwrap(),
            OpenTarget::File(PathBuf::from("/var/lib/app.db"))
        );
        // A `file://` URL on another host names no local path.
        assert!(classify(OsStr::new("file://elsewhere/app.db")).is_err());
    }

    #[test]
    fn an_unusable_url_reports_the_problem_without_connecting() {
        // Port 0 and a bare scheme are rejected by the shared URL validation;
        // the message must not pretend a connection was attempted.
        let err = classify(OsStr::new("postgres://user@host:0/app")).unwrap_err();
        assert!(err.to_string().contains("port"), "{err}");
        assert!(
            !err.to_string().contains("connection failed"),
            "nothing has been connected: {err}"
        );
    }

    #[test]
    fn unknown_options_and_extra_arguments_are_usage_errors() {
        let err = parse(args(&["--frobnicate"])).unwrap_err();
        assert!(err.to_string().contains("--frobnicate"), "{err}");
        assert_eq!(err.exit_code(), 2);
        assert!(err.is_usage());
        let err = parse(args(&["a.db", "b.db"])).unwrap_err();
        assert_eq!(err, CliError::TooManyArguments);
        assert_eq!(err.exit_code(), 2);
        // A file problem is a failure, not a usage mistake — pointing at
        // `--help` would be no help at all.
        let err = CliError::File(SqliteFileError::not_found(PathBuf::from("/nope.db")));
        assert_eq!(err.exit_code(), 1);
        assert!(!err.is_usage());
    }

    #[test]
    fn double_dash_reaches_a_file_named_like_an_option() {
        assert_eq!(
            parse(args(&["--", "--weird.db"])).unwrap(),
            Invocation::Run(Some(OpenTarget::File(PathBuf::from("--weird.db"))))
        );
        // …and only the first `--` is special; a second is just a file name.
        assert_eq!(
            parse(args(&["--", "--"])).unwrap(),
            Invocation::Run(Some(OpenTarget::File(PathBuf::from("--"))))
        );
    }

    #[test]
    fn redaction_removes_the_password_and_nothing_else() {
        assert_eq!(
            redact_url("postgres://user:hunter2@host:5432/app?sslmode=require"),
            "postgres://user:***@host:5432/app?sslmode=require"
        );
        // The password may hold an encoded `@`; the *last* one delimits it.
        assert_eq!(
            redact_url("postgres://user:a%40b@host/app"),
            "postgres://user:***@host/app"
        );
        // A password with no user, and a URL with no path.
        assert_eq!(redact_url("mssql://:pw@host"), "mssql://:***@host");
        // Nothing to redact: returned byte-for-byte.
        for text in [
            "postgres://user@host:5432/app",
            "mssql://host/app",
            "/var/lib/app.db",
            "no such file: /home/me/notes.txt",
            "",
            "://",
        ] {
            assert_eq!(redact_url(text), text, "{text}");
        }
    }

    #[test]
    fn a_display_name_is_built_from_components_never_from_the_url() {
        assert_eq!(
            display_name("postgres://u@db.example.com:5432/app"),
            "app@db.example.com"
        );
        assert_eq!(
            display_name("mssql://sa@db.example.com:1433"),
            "db.example.com"
        );
        assert_eq!(display_name("not a url"), "database");
        // Even handed a URL that still carries one, the name cannot contain it
        // — it is assembled from the host and path, and never from the text.
        assert_eq!(
            display_name("postgres://u:hunter2@db.example.com:5432/app"),
            "app@db.example.com"
        );
    }

    #[test]
    fn url_scheme_only_matches_a_real_scheme() {
        assert_eq!(url_scheme("postgres://h/db"), Some("postgres"));
        assert_eq!(url_scheme("ms-sql+tds://h"), Some("ms-sql+tds"));
        assert_eq!(url_scheme("/var/lib/app.db"), None);
        assert_eq!(url_scheme(r"C:\db\app.db"), None);
        // A scheme must start with a letter, so this is a (very odd) path.
        assert_eq!(url_scheme("2fast://h"), None);
        assert_eq!(url_scheme("://h"), None);
        assert_eq!(url_scheme("dir/sub://weird"), None);
    }

    #[test]
    fn only_the_first_taker_gets_the_opened_queue() {
        // A second holder would silently swallow the OS's deliveries.
        let first = take_opened();
        assert!(first.is_some());
        assert!(take_opened().is_none());
    }
}
