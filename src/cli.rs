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
//!   with the password carried beside it, to be tried once and remembered only
//!   if it works (see `AppState::open_target`).
//! - [`display_name`] builds a tab/entry name from the URL's host and database
//!   components rather than from the URL text, so the name that *is* persisted
//!   cannot contain a credential.
//!
//! `tests/cli_secrets.rs` holds those claims to a password-bearing URL pushed
//! through every one of these paths.

use std::ffi::{OsStr, OsString};
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
#[cfg(test)]
use std::time::Duration;

use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};

use crate::config::BackendKind;
use crate::db::{
    check_sqlite_file, normalize_mssql_url, normalize_pg_url, DbError, SqliteFileError,
};

/// The file extensions hubro registers itself as a handler for.
///
/// The single source of truth for the two packaging declarations that name
/// extensions — the macOS `CFBundleDocumentTypes` and the Windows registry
/// fragment — which must list exactly these; `tests/file_associations.rs`
/// fails if either drifts. Linux names a MIME type instead and claims no
/// extension at all, for the reason measured in `packaging/README.md`.
///
/// `db` is deliberately included even though it is a contested extension —
/// hubro registers as *a* handler for it, never as the default.
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
        /// asks for).
        ///
        /// Nothing has validated it, so it is never treated as a remembered
        /// secret: `AppState::open_target` connects with it directly and it
        /// enters session memory only once a connect has accepted it.
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
    /// An option hubro doesn't have. Holds the option *name* only — see
    /// [`CliError::unknown_option`], which is the only way to build it.
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
    /// Builds [`CliError::UnknownOption`] from the argument as typed, keeping
    /// only the option *name*: everything before the first `=` or whitespace.
    ///
    /// The value of an option hubro does not have is of no use in the message
    /// — "unknown option `--connect`" names the mistake completely — and this
    /// is the one place an arbitrary user string was echoed back.
    /// `--connect=<a URL with a password>` is not a contrived shape: it is
    /// what a `psql` habit produces.
    ///
    /// Whitespace ends the name as well as `=`, for a reason beyond tidiness:
    /// it is precisely the boundary [`redact_url`] cannot see past. A single
    /// argument like `-x <url>` — quoted, so the shell does not split it —
    /// would otherwise carry a password containing a space into a message that
    /// has no way to redact one. Truncating here means the error never *holds*
    /// the secret, which is the stronger guarantee; redaction still runs on
    /// the name as defence in depth.
    pub fn unknown_option(arg: &str) -> CliError {
        let name = arg
            .split_once(|c: char| c == '=' || c.is_whitespace())
            .map_or(arg, |(name, _)| name);
        CliError::UnknownOption(name.to_string())
    }

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

/// Replaces the password in every `scheme://user:password@…` run in `text`
/// with `***`.
///
/// Works on the raw string rather than on a parsed [`url::Url`], because the
/// strings that most need redacting are the ones that failed to parse — an
/// error message about a malformed URL must not quote the malformed URL back
/// with its password intact.
///
/// It therefore must not assume the text *is* a well-formed URL. An earlier
/// version bounded the userinfo by the first `/`, `?` or `#` after the scheme,
/// on the reasoning that those end a URL's authority. That is true of a valid
/// URL and false of the input this is for: a password containing an unencoded
/// `/` — `postgres://user:hun/ter@host/db`, which is what someone types when
/// their password has a slash in it — ended the search before the `@`, so no
/// userinfo was found and the secret was returned verbatim. Whatever bounds
/// the search has to be something a password genuinely cannot contain.
///
/// So the run is bounded by **whitespace**, which no URL may contain
/// unescaped, and the userinfo is whatever lies between the scheme and the
/// last `@` in that run. That over-reaches rather than under-reaches: it will
/// redact `scheme://a:b@c` inside a sentence, which is the direction to err in.
///
/// The one shape it does not catch is a password containing whitespace, which
/// ends the run before the `@` is reached. That gap is real and cannot be
/// closed here — bounding the search by anything a password *may* contain is
/// what caused the previous bug, and whitespace is the last character class
/// left. (It is not, as an earlier version of this comment claimed, ruled out
/// by `url::Url::parse`: the url crate percent-encodes a space in the
/// userinfo, so `postgres://user:SEK RET@host/db` parses and connects
/// perfectly well.)
///
/// It is closed one level up instead, by no error variant holding raw user
/// text that reaches past a space: [`CliError::unknown_option`] truncates at
/// the first `=` *or* whitespace, [`CliError::UnsupportedScheme`] holds only a
/// scheme, [`CliError::UnusableUrl`] holds only validation messages that never
/// quote their input, and [`CliError::File`] holds a path. Anything added to
/// [`CliError`] that echoes an argument has to keep that true — redaction
/// alone will not.
pub fn redact_url(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(scheme_end) = rest.find("://") {
        let authority_start = scheme_end + 3;
        out.push_str(&rest[..authority_start]);
        rest = &rest[authority_start..];
        // A URL cannot contain unescaped whitespace, so the surrounding prose
        // — not a `/` that may well be part of the secret — is what bounds it.
        let run = &rest[..rest.find(char::is_whitespace).unwrap_or(rest.len())];
        // Userinfo ends at the *last* `@` in the run: `@` is legal inside a
        // percent-encoded password, and taking the first would leave the tail
        // of the secret in place.
        let Some(at) = run.rfind('@') else {
            continue; // no userinfo here; keep scanning for a later URL
        };
        let Some(colon) = run[..at].find(':') else {
            continue; // a user with no password
        };
        out.push_str(&run[..colon]);
        out.push_str(":***");
        rest = &rest[at..];
    }
    out.push_str(rest);
    out
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
                Some(flag) if flag.starts_with('-') => return Err(CliError::unknown_option(flag)),
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

/// Yields the databases to open, in the order they must be opened: the
/// command-line target first, then whatever the OS hands over, one at a time,
/// for as long as the app runs.
///
/// A function rather than two `spawn`s so the ordering is a property of a
/// single loop instead of a race between tasks. The queue used to be drained
/// by a task of its own, started in parallel with the session restore — which
/// on macOS, the only platform that delivers through it, meant a
/// double-clicked database could be opened *during* the restore and then lose
/// the foreground to the tab the restore activates last. That is exactly the
/// failure the command-line path orders itself to avoid, and it does not
/// survive being written as one sequence.
///
/// Both sources are consumed in place: `startup` is taken on the first call,
/// and `opened` yields until the queue closes. Returns `None` when neither can
/// produce anything again, which ends the caller's loop.
pub async fn next_startup_target(
    startup: &mut Option<OpenTarget>,
    opened: &mut Option<UnboundedReceiver<OpenTarget>>,
) -> Option<OpenTarget> {
    if let Some(target) = startup.take() {
        return Some(target);
    }
    let receiver = opened.as_mut()?;
    match receiver.recv().await {
        Some(target) => Some(target),
        None => {
            // The sender is a `'static` in this process, so this only happens
            // in tests; dropping the receiver keeps the caller's loop from
            // spinning on a closed queue.
            *opened = None;
            None
        }
    }
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

    #[tokio::test]
    async fn the_command_line_target_opens_before_anything_the_os_delivers() {
        // The ordering this function exists for. The queue used to be drained
        // by a task of its own, racing the session restore; the restore ends by
        // activating whichever tab was in front last time, so anything opened
        // alongside it silently loses the foreground. Sequencing every source
        // through one function is what makes "after the restore, then argv,
        // then the OS queue, one at a time" a property of the code rather than
        // of a comment.
        let (tx, rx) = unbounded_channel();
        let queued = |name: &str| OpenTarget::File(PathBuf::from(name));
        tx.send(queued("first-delivered.db")).unwrap();
        tx.send(queued("second-delivered.db")).unwrap();
        drop(tx); // no more deliveries, so the loop can end

        let mut startup = Some(OpenTarget::File(PathBuf::from("argv.db")));
        let mut opened = Some(rx);
        let mut order = Vec::new();
        while let Some(target) = next_startup_target(&mut startup, &mut opened).await {
            order.push(target.to_string());
        }
        assert_eq!(
            order,
            ["argv.db", "first-delivered.db", "second-delivered.db"],
            "the command line comes first, then the OS queue in arrival order"
        );
        // Both sources are spent: the caller's loop ends rather than spinning.
        assert!(startup.is_none());
        assert!(opened.is_none());
    }

    #[tokio::test]
    async fn each_source_works_without_the_other() {
        // A plain `hubro` on a platform that never delivers an Opened event —
        // every launch on Linux and Windows — has neither source, and must not
        // hang waiting on a queue that will never speak.
        let mut startup = None;
        let mut opened = None;
        assert!(next_startup_target(&mut startup, &mut opened)
            .await
            .is_none());

        // Only a command-line target.
        let mut startup = Some(OpenTarget::File(PathBuf::from("only.db")));
        let mut opened = None;
        assert_eq!(
            next_startup_target(&mut startup, &mut opened)
                .await
                .map(|t| t.to_string()),
            Some("only.db".to_string())
        );
        assert!(next_startup_target(&mut startup, &mut opened)
            .await
            .is_none());

        // Only a delivery (macOS double-click with no argument).
        let (tx, rx) = unbounded_channel();
        tx.send(OpenTarget::File(PathBuf::from("delivered.db")))
            .unwrap();
        drop(tx);
        let mut startup = None;
        let mut opened = Some(rx);
        assert_eq!(
            next_startup_target(&mut startup, &mut opened)
                .await
                .map(|t| t.to_string()),
            Some("delivered.db".to_string())
        );
        assert!(next_startup_target(&mut startup, &mut opened)
            .await
            .is_none());
    }

    #[tokio::test]
    async fn a_delivery_that_arrives_later_is_still_yielded() {
        // The launch-by-double-click case on macOS is the *early* delivery, but
        // the queue also has to stay live afterwards: hubro is a running app
        // and the OS can hand it another file at any time.
        let (tx, rx) = unbounded_channel();
        let mut startup = None;
        let mut opened = Some(rx);
        let sender = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            tx.send(OpenTarget::File(PathBuf::from("late.db"))).unwrap();
        });
        assert_eq!(
            next_startup_target(&mut startup, &mut opened)
                .await
                .map(|t| t.to_string()),
            Some("late.db".to_string())
        );
        sender.await.unwrap();
    }
}
