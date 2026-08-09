use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use dioxus::core::{spawn_forever, Task};
use dioxus::prelude::*;

use crate::azure::{self, EntraAuth};
use crate::config::{
    default_config_path, default_session_path, default_settings_path, load_session, load_settings,
    plan_session_restore, save_session, save_show_internal_objects, save_theme, BackendKind,
    ConnectionColor, PgAuth, RestoreCandidate, SavedConnection, SavedList, Session, SessionPane,
    SessionTab, Theme,
};
use crate::db::{
    apply_staged, build_fk_filter, mssql_url_target, mssql_url_via_local_port,
    mssql_url_with_password, needs_confirmation, run_script, script_refusal, split_statements,
    statement_preview, url_target, url_via_local_port, url_with_password, write_result,
    Capabilities, CellFetch, Connection, ConnectionId, ConnectionRegistry, DbError, DbPool, Ddl,
    DdlObject, ExportFormat, Filter, ForeignKeyMeta, MssqlAuth, QueryResult, RowLocator,
    StagedChange, StatementResult, TableAccess, TableMeta, Value, WriteProtection,
};
use crate::history::HistoryStore;
use crate::tunnel::{HostKeyInfo, Tunnel, TunnelAuth, TunnelConfig, TunnelError};
use crate::ui::notice::SPINNER_DELAY;
use crate::ui::stage::TableStage;

/// Which screen the main panel shows: the connections screen or one open
/// connection tab.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActiveView {
    Connections,
    Connection(ConnectionId),
}

/// Schema introspection state for one connection.
#[derive(Debug, Clone, PartialEq)]
pub enum SchemaLoad {
    Loading,
    Ready(Vec<TableMeta>),
    Failed(String),
}

/// A table identified by optional schema + name (schema is `None` on
/// SQLite).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TableRef {
    pub schema: Option<String>,
    pub name: String,
}

impl TableRef {
    /// Stable string form, used as component keys and expansion-set
    /// entries. Debug-formats both parts so schema/name splits can't
    /// collide (schema "a" + table "b.c" vs schema "a.b" + table "c").
    pub fn key(&self) -> String {
        format!("{:?}:{:?}", self.schema, self.name)
    }

    /// Human-readable qualified name.
    pub fn label(&self) -> String {
        match &self.schema {
            Some(schema) => format!("{schema}.{}", self.name),
            None => self.name.clone(),
        }
    }
}

/// A table plus the filter to show it under — the payload of a foreign-key
/// jump or a Back restore ([`AppState::navigate_fk`] /
/// [`AppState::navigate_back`]). The grid consumes a pending focus that
/// targets its table, seeding its filter from it (see
/// [`AppState::pending_focus`]).
#[derive(Debug, Clone, PartialEq)]
pub struct FocusTarget {
    pub table: TableRef,
    /// `None` restores an unfiltered view (a Back to a table the user was
    /// browsing without a filter).
    pub filter: Option<Filter>,
}

/// A saved-connection edit that has been submitted but not yet confirmed by
/// a successful connect (FRE-75). Matched on `new_locator` so an abandoned
/// edit can never rewrite an unrelated connection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingEdit {
    pub old_locator: String,
    pub new_locator: String,
}

/// Which pane a connection tab shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Pane {
    #[default]
    Browser,
    Sql,
    /// The selected table's structure — columns and indexes (FRE-69).
    Schema,
}

impl Pane {
    /// The serializable form used in the persisted session (FRE-30).
    fn to_session(self) -> SessionPane {
        match self {
            Pane::Browser => SessionPane::Browser,
            Pane::Sql => SessionPane::Sql,
            Pane::Schema => SessionPane::Schema,
        }
    }

    fn from_session(pane: SessionPane) -> Self {
        match pane {
            SessionPane::Browser => Pane::Browser,
            SessionPane::Sql => Pane::Sql,
            SessionPane::Schema => Pane::Schema,
        }
    }
}

/// Minimum age of a parked navigation intent before repeating the action
/// confirms it. Without a floor, a double-click delivers two identical
/// attempts ~100 ms apart — parking and immediately "confirming" the
/// discard the user never read. Repeats inside the floor are ignored (the
/// original park time is kept, so a deliberate later repeat still confirms).
const NAV_CONFIRM_MIN_DELAY: Duration = Duration::from_millis(500);

/// A navigation the unsaved-changes guard intercepted (see
/// [`AppState::nav_guard`]).
#[derive(Debug, Clone)]
pub struct PendingNav {
    pub id: ConnectionId,
    pub action: NavAction,
    /// When the intent was parked (drives the double-click floor).
    parked_at: Instant,
}

impl PendingNav {
    fn new(id: ConnectionId, action: NavAction) -> Self {
        PendingNav {
            id,
            action,
            parked_at: Instant::now(),
        }
    }

    /// Whether a new attempt repeats this parked intent.
    fn matches(&self, id: ConnectionId, action: &NavAction) -> bool {
        self.id == id && self.action == *action
    }

    /// Whether the intent is old enough for a repeat to confirm it.
    fn confirmable(&self) -> bool {
        self.parked_at.elapsed() >= NAV_CONFIRM_MIN_DELAY
    }
}

/// The navigations guarded against unsaved staged edits.
#[derive(Debug, Clone, PartialEq)]
pub enum NavAction {
    SelectTable(TableRef),
    SetPane(Pane),
    CloseConnection,
}

/// Per-tab UI state that must survive tab switches.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct TabUi {
    /// Table selected in the sidebar (shown in the data grid).
    pub selected_table: Option<TableRef>,
    /// Data browser, SQL editor, or schema.
    pub pane: Pane,
    /// SQL editor buffer, synced from the webview so it survives pane and
    /// tab switches.
    pub sql_text: String,
    /// Whether the row detail panel is docked open beside the grid
    /// (FRE-109). Per tab rather than per table, so it stays open while
    /// browsing from table to table; persisted in the session.
    pub row_detail: bool,
    /// The row detail panel's dragged width in CSS pixels, or `None` for the
    /// default. Kept here (not in the grid) so a table switch — which
    /// remounts the grid — doesn't snap the panel back. Deliberately not
    /// persisted: the session remembers whether the panel is open, and a
    /// width is cheap to re-drag.
    pub row_detail_width: Option<f64>,
}

/// Where a script run currently stands. Per-statement outcomes accumulate
/// in [`SqlRun::statements`] as they finish; the status carries the
/// script-level timing and failure info.
#[derive(Debug, Clone, PartialEq)]
pub enum RunStatus {
    Running,
    Done {
        elapsed_ms: u64,
    },
    /// The statement at `statement_index` (0-based, into the script)
    /// failed. Outcomes of the statements before it stay visible in
    /// [`SqlRun::statements`]. `rolled_back` distinguishes an atomic run (the
    /// whole script was undone) from a sequential one (earlier statements
    /// persisted).
    Failed {
        error: String,
        statement_index: usize,
        preview: String,
        elapsed_ms: u64,
        rolled_back: bool,
    },
    /// The connection's capabilities forbid the statement at
    /// `statement_index` (FRE-87), so **nothing was sent**. Distinct from
    /// [`Self::Failed`]: there is no partial state to explain and no timing
    /// to report, because no statement ran.
    Refused {
        reason: String,
        statement_index: usize,
        preview: String,
    },
    /// The user aborted the run. Outcomes of the statements that finished
    /// before the abort stay visible; the in-flight statement may still
    /// complete server-side (see [`AppState::cancel_sql`]).
    Cancelled,
}

/// One finished statement's result, shared rather than owned: a result can
/// hold thousands of rows, and the editor re-reads the run on every render,
/// so cloning it must cost an `Arc` bump, not a row-by-row copy (FRE-134).
///
/// `PartialEq` is pointer identity, not a deep compare. That is sound because
/// results are write-once — pushed into [`SqlRun::statements`] and never
/// mutated — so two equal pointers are the same result and two different
/// pointers belong to different runs. It is also the point: pointer equality
/// is what lets prop diffing on the result components short-circuit without
/// walking every row (a derived `PartialEq` on `Arc` deep-compares, since
/// row values hold floats and so are only `PartialEq`, not `Eq`).
#[derive(Debug, Clone)]
pub struct SharedStatement(Arc<StatementResult>);

impl SharedStatement {
    pub fn new(result: StatementResult) -> Self {
        Self(Arc::new(result))
    }
}

impl PartialEq for SharedStatement {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl std::ops::Deref for SharedStatement {
    type Target = StatementResult;

    fn deref(&self) -> &StatementResult {
        &self.0
    }
}

/// State of the most recent SQL script run per connection.
#[derive(Debug, Clone, PartialEq)]
pub struct SqlRun {
    /// Outcomes of the statements that finished, in script order.
    pub statements: Vec<SharedStatement>,
    pub status: RunStatus,
}

/// Which pane an export belongs to. Statuses are keyed per connection AND
/// per pane so a grid export and a SQL-result export never overwrite each
/// other's toolbar line (FRE-73).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ExportPane {
    Grid,
    Sql,
}

/// Progress of the most recent export in one pane of one connection.
/// Shown as a small transient line in the pane's toolbar; it stays until the
/// next export replaces it.
#[derive(Debug, Clone, PartialEq)]
pub enum ExportStatus {
    Running,
    Done { rows: u64 },
    Failed(String),
}

impl ExportStatus {
    /// The toolbar line for this status: display text plus a Tailwind color
    /// class. Shared by the grid and SQL-editor export controls.
    pub fn line(&self) -> (String, &'static str) {
        match self {
            ExportStatus::Running => (
                "Exporting…".to_string(),
                "text-slate-500 dark:text-slate-400",
            ),
            ExportStatus::Done { rows } => (
                format!("Exported {rows} row{}", if *rows == 1 { "" } else { "s" }),
                "text-emerald-700 dark:text-emerald-400",
            ),
            ExportStatus::Failed(err) => (
                format!("Export failed: {err}"),
                "text-red-600 dark:text-red-400",
            ),
        }
    }
}

/// A script held back by the write-confirmation banner: the original text
/// (recorded into history when the run happens) plus its split statements.
#[derive(Debug, Clone, PartialEq)]
pub struct PendingSql {
    pub script: String,
    pub statements: Vec<String>,
}

/// What a pending [`PasswordPrompt`] is asking for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptKind {
    /// The database password.
    DbPassword,
    /// The passphrase decrypting the SSH tunnel's key file.
    SshPassphrase,
}

/// A pending secret request for a saved Postgres or SQL Server connection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PasswordPrompt {
    pub url: String,
    pub name: String,
    pub kind: PromptKind,
    /// Which backend the retry must reconnect with (Postgres or SqlServer —
    /// SQLite never prompts).
    pub backend: BackendKind,
    /// Tunnel settings of the connect attempt, carried through the prompt so
    /// the retry resumes the same flow.
    pub tunnel: Option<TunnelConfig>,
    /// Auth mode of the attempt, carried through so the retry (e.g. after an
    /// SSH passphrase) resumes the same Entra/password flow.
    pub auth: PgAuth,
}

/// A pending host-key trust decision for a tunneled Postgres or SQL Server
/// connect. The server presented a key not yet in known_hosts; the connect is
/// parked here until the user trusts it (persist + retry) or cancels.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostKeyPrompt {
    pub url: String,
    pub name: String,
    /// Tunnel settings of the attempt, carried through so the retry resumes.
    pub tunnel: TunnelConfig,
    /// The offered key: what to display and, on trust, what to persist.
    pub info: HostKeyInfo,
    /// Auth mode of the attempt, carried through so the retry resumes it.
    pub auth: PgAuth,
    /// Which backend the retry must reconnect with.
    pub backend: BackendKind,
}

/// A pending interactive Microsoft Entra sign-in for a Postgres or SQL Server
/// connection: the connect needs a browser sign-in (no cached refresh token),
/// so it's parked here until the user starts the sign-in or cancels (FRE-44).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EntraPrompt {
    pub url: String,
    pub name: String,
    pub tunnel: Option<TunnelConfig>,
    pub entra: EntraAuth,
    /// Which backend the sign-in retry must reconnect with (routes the token's
    /// OAuth resource and the driver, FRE-58).
    pub backend: BackendKind,
}

/// Moves one keyring secret from `old` to `new` (FRE-75). Best-effort: a
/// missing secret, or a keyring that refuses, just means nothing to carry.
/// An existing secret under `new` is left alone — it came from the connect
/// that just succeeded and is therefore the more current one.
async fn migrate_secret(old: String, new: String) {
    if old == new {
        return;
    }
    let Ok(Some(secret)) = crate::secrets::get_password_async(old.clone()).await else {
        return;
    };
    match crate::secrets::get_password_async(new.clone()).await {
        // Nothing under the new key yet: carry the secret across, and only
        // drop the old copy once the new one is safely written — deleting
        // after a failed store would lose the password outright.
        Ok(None) => {
            if crate::secrets::store_password_async(new, secret)
                .await
                .is_ok()
            {
                let _ = crate::secrets::delete_password_async(old).await;
            }
        }
        // The connect that just succeeded already wrote a newer secret;
        // the old one is now redundant.
        Ok(Some(_)) => {
            let _ = crate::secrets::delete_password_async(old).await;
        }
        // Keyring unreadable — leave both alone rather than risk the only
        // copy.
        Err(_) => {}
    }
}

/// Keyring/session key for a connection's SSH key passphrase. The `#ssh`
/// suffix keeps it disjoint from the database password stored under the
/// bare URL (`#` cannot appear in a valid connection URL's serialized form
/// unescaped, so this never collides).
pub(crate) fn ssh_secret_key(url: &str) -> String {
    format!("{url}#ssh")
}

/// Keyring key for a connection's cached Entra refresh token. Disjoint from the
/// password (bare URL) and SSH passphrase (`#ssh`) keys, so the three never
/// collide. Only a refresh token is ever cached here — never an access token.
pub(crate) fn entra_secret_key(url: &str) -> String {
    format!("{url}#entra")
}

/// A secret to hand a server backend at connect time. Which variant a connect
/// carries decides how the backend uses it: Postgres takes a password and an
/// Entra token the same way — spliced into the URL — while SQL Server splices
/// the password but hands the token to the driver as an AAD login, since a
/// token sent as a password is just a rejected password.
enum ServerCredential<'a> {
    /// Nothing known: connect with the URL as-is. Trust auth succeeds; a
    /// server that wants a password answers with the failure that raises the
    /// password prompt.
    None,
    /// The database password — from session memory, the OS keyring, or the
    /// prompt.
    Password(&'a str),
    /// A Microsoft Entra access token (never a refresh token).
    EntraToken(&'a str),
}

/// The per-engine data the server connect flows are parameterized over.
///
/// Postgres and SQL Server connect through the same sequence — reserve, open
/// the tunnel, find a credential, open the pool, prompt or save — and used to
/// carry two line-for-line copies of it (FRE-139). They differ only in engine
/// *data*: which URL helpers apply, which OAuth resource Entra tokens are
/// minted for, whether a tunneled connect needs a TLS host override, and which
/// [`SavedConnection`] variant a success persists. That data lives here and the
/// flow is written once, the same split [`crate::db`]'s `UrlScheme` descriptor
/// makes one layer down.
///
/// `Copy`, and passed by value: every connect flow is an `async fn` on
/// [`AppState`] that may be spawned onto a root task, and a plain value avoids
/// borrowing a descriptor across those awaits.
#[derive(Clone, Copy)]
pub struct ServerBackend {
    /// Which backend this is. Carried into the prompts, which park a connect
    /// across a UI round-trip and must resume it on the same engine.
    pub kind: BackendKind,
    /// Splices a secret into a URL as its password.
    with_password: fn(&str, &str) -> Result<String, DbError>,
    /// The host and port an SSH tunnel must forward to.
    url_target: fn(&str) -> Result<(String, u16), DbError>,
    /// Rewrites a URL onto the tunnel's forwarded local port.
    via_local_port: fn(&str, u16) -> Result<String, DbError>,
    /// The OAuth resource this engine's Entra tokens are minted for.
    entra_resource: &'static str,
}

impl ServerBackend {
    pub const POSTGRES: ServerBackend = ServerBackend {
        kind: BackendKind::Postgres,
        with_password: url_with_password,
        url_target,
        via_local_port: url_via_local_port,
        entra_resource: azure::OSSRDBMS_RESOURCE,
    };

    pub const SQL_SERVER: ServerBackend = ServerBackend {
        kind: BackendKind::SqlServer,
        with_password: mssql_url_with_password,
        url_target: mssql_url_target,
        via_local_port: mssql_url_via_local_port,
        entra_resource: azure::SQLDB_RESOURCE,
    };

    /// The descriptor for a saved entry's or a prompt's backend. SQLite maps
    /// to Postgres because it never reaches these flows at all — it has no
    /// URL, no auth mode and no prompts — which is exactly what the
    /// `if backend == SqlServer { … } else { … }` dispatch these flows
    /// replaced did with it.
    pub fn of(kind: BackendKind) -> ServerBackend {
        match kind {
            BackendKind::SqlServer => ServerBackend::SQL_SERVER,
            _ => ServerBackend::POSTGRES,
        }
    }

    /// The TLS host override a connect needs, or `None` when the engine has no
    /// such notion.
    ///
    /// Only SQL Server does, and only through a tunnel: the connect URL then
    /// points at `127.0.0.1:<forwarded>`, but `encrypt=on` must keep
    /// validating the server's real certificate (see
    /// [`crate::db::open_mssql_with`]). The URL is parsed again here, which
    /// the tunnel open already did, so the fallible parse cannot practically
    /// fail. Postgres returns `None` unconditionally — sqlx validates against
    /// the URL's own host, and there is no parameter to leak this into.
    fn tls_host(&self, url: &str, tunneled: bool) -> Option<String> {
        match self.kind {
            BackendKind::SqlServer if tunneled => (self.url_target)(url).ok().map(|(host, _)| host),
            _ => None,
        }
    }

    /// Opens the pool with `credential`, splicing it into the URL or handing
    /// it to the driver as this engine requires.
    ///
    /// A `match` on the kind rather than another function pointer in the
    /// descriptor: the two openers differ in *shape*, not just identity (SQL
    /// Server takes an auth mode and a TLS host beside the URL), and an async
    /// function pointer would have to be boxed per call while still needing
    /// this argument juggling somewhere.
    async fn open(
        &self,
        connect_url: &str,
        credential: ServerCredential<'_>,
        tls_host: Option<&str>,
    ) -> Result<DbPool, DbError> {
        match self.kind {
            BackendKind::SqlServer => {
                let (url, auth) = match credential {
                    ServerCredential::None => (connect_url.to_string(), MssqlAuth::Password),
                    ServerCredential::Password(password) => (
                        (self.with_password)(connect_url, password)?,
                        MssqlAuth::Password,
                    ),
                    // tiberius logs in with the token itself; the URL's
                    // user/password are ignored, so nothing is spliced in.
                    ServerCredential::EntraToken(token) => (
                        connect_url.to_string(),
                        MssqlAuth::AadToken(token.to_string()),
                    ),
                };
                DbPool::open_mssql_with(&url, &auth, tls_host).await
            }
            // Postgres takes both secrets the same way, and has no TLS host
            // override to pass.
            _ => {
                let url = match credential {
                    ServerCredential::None => connect_url.to_string(),
                    ServerCredential::Password(secret) | ServerCredential::EntraToken(secret) => {
                        (self.with_password)(connect_url, secret)?
                    }
                };
                DbPool::open_postgres(&url).await
            }
        }
    }

    /// The saved-list entry a successful connect persists.
    fn saved(
        &self,
        name: &str,
        url: &str,
        tunnel: Option<TunnelConfig>,
        auth: PgAuth,
    ) -> SavedConnection {
        let name = name.to_string();
        let url = url.to_string();
        match self.kind {
            BackendKind::SqlServer => SavedConnection::SqlServer {
                name,
                url,
                tunnel,
                auth,
                protection: WriteProtection::Open,
                color: None,
            },
            _ => SavedConnection::Postgres {
                name,
                url,
                tunnel,
                auth,
                protection: WriteProtection::Open,
                color: None,
            },
        }
    }
}

/// Acquires one Entra token for `url`'s connection: redeem the cached refresh
/// token when there is one, otherwise fall through to `open_browser`.
///
/// The four token acquisitions in the connect flows (two engines × silent and
/// interactive) differ only in the resource and that opener, so this is the one
/// place the keyring's cached refresh token is read. A keyring error means "no
/// cached token" and falls through, exactly as an absent one does; only a
/// refresh token is ever stored under [`entra_secret_key`], never an access
/// token.
async fn acquire_entra(
    entra: &EntraAuth,
    resource: &str,
    url: &str,
    open_browser: impl FnOnce(&str) -> Result<(), azure::AzureError>,
) -> Result<azure::AccessToken, azure::AzureError> {
    let cached = crate::secrets::get_password_async(entra_secret_key(url))
        .await
        .ok()
        .flatten();
    azure::acquire_token(
        entra,
        resource,
        cached.as_deref(),
        &azure::Endpoints::default(),
        azure::INTERACTIVE_TIMEOUT,
        open_browser,
    )
    .await
}

/// The opener a *silent* acquisition passes: it refuses to open a browser, so
/// an interactive sign-in with no usable refresh token errors here and the
/// flow can park the sign-in card instead of popping a window unasked.
fn refuse_browser(_url: &str) -> Result<(), azure::AzureError> {
    Err(azure::AzureError::Browser(
        "interactive sign-in required".to_string(),
    ))
}

/// The opener the sign-in card passes: the user asked for the browser.
fn open_browser(auth_url: &str) -> Result<(), azure::AzureError> {
    webbrowser::open(auth_url)
        .map(|_| ())
        .map_err(|e| azure::AzureError::Browser(e.to_string()))
}

/// Whether any table stage anywhere in the staged map has pending edits. Pure
/// so the window-close guard's predicate can be unit-tested without a running
/// reactive context.
fn staged_has_dirty(staged: &HashMap<ConnectionId, HashMap<String, TableStage>>) -> bool {
    staged
        .values()
        .any(|tables| tables.values().any(|stage| !stage.is_empty()))
}

/// Which phase of a connect is running, so the connections list can say more
/// than "working". The slow, hang-prone steps are the ones worth naming: a
/// tunnel to an unreachable jump host and a locked keyring both stall for a
/// long time, and "connecting…" alone gives the user nothing to act on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectStep {
    /// Bringing up the SSH tunnel the connection goes through.
    Tunnel,
    /// Reading the saved password from this session or the OS keyring — a
    /// locked wallet blocks here until the user answers its own dialog.
    Credentials,
    /// Acquiring a Microsoft Entra token.
    SigningIn,
    /// Opening the database connection itself.
    Opening,
}

impl ConnectStep {
    /// Lowercase, trailing ellipsis: this renders as a status line under the
    /// connection name, not as a sentence.
    pub fn label(self) -> &'static str {
        match self {
            ConnectStep::Tunnel => "opening SSH tunnel…",
            ConnectStep::Credentials => "reading saved password…",
            ConnectStep::SigningIn => "signing in…",
            ConnectStep::Opening => "connecting…",
        }
    }
}

/// The key a connect is tracked under: the locator as an open tab would
/// report it (what [`AppState::connect`] stores in `open_locators`). Only
/// SQLite differs from the saved form — its locator is the canonicalized
/// path, so a row keyed on the saved spelling would never match its own
/// in-flight progress.
pub fn connect_key(locator: &str, backend: BackendKind) -> String {
    match backend {
        BackendKind::Sqlite => canonical(Path::new(locator)).display().to_string(),
        _ => locator.to_string(),
    }
}

/// A connect in flight, from the reservation until the tab opens or the
/// attempt fails.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Connecting {
    /// Canonical locator, the same key [`AppState::open_locators`] uses.
    pub locator: String,
    pub step: ConnectStep,
    /// Whether the row should show progress yet. Stays false for
    /// [`SPINNER_DELAY`] so a local SQLite open — which finishes in
    /// milliseconds — never flashes a spinner.
    pub visible: bool,
}

/// A connect started from the connections list, as opposed to one started by
/// submitting a form: the list can cancel it, and a shift-click asks for the
/// tab to open without stealing focus.
#[derive(Clone, Copy)]
pub struct ConnectRequest {
    /// Root-scope task running the connect. Cancelling drops it mid-await,
    /// which drops any half-open tunnel with it.
    task: Task,
    /// False for a shift-click: open the tab, but stay on the list.
    focus: bool,
}

/// App-wide state provided via context. `Copy` because it only holds signals.
#[derive(Clone, Copy)]
pub struct AppState {
    pub registry: Signal<ConnectionRegistry>,
    pub active: Signal<ActiveView>,
    /// Saved connections shown on the launch screen.
    pub saved: Signal<SavedList>,
    /// Locator (canonical file path / URL) each open tab came from, for
    /// "already open" detection.
    pub open_locators: Signal<Vec<(ConnectionId, String)>>,
    /// Connects in flight, reserved before the pool open await. Drives the
    /// connections list's per-row progress.
    pub connecting: Signal<Vec<Connecting>>,
    /// Cancel handle and focus intent for connects started from the
    /// connections list, keyed by locator. Connects started by submitting a
    /// form have no entry — they still show progress, just no cancel.
    pub connect_requests: Signal<HashMap<String, ConnectRequest>>,
    /// Error from the most recent connect/config operation, shown on the
    /// connections screen.
    pub connect_error: Signal<Option<String>>,
    /// Secrets entered this session: Postgres passwords keyed by stored URL,
    /// SSH key passphrases keyed by [`ssh_secret_key`]. Never persisted here
    /// — the OS keyring handles "remember".
    pub session_passwords: Signal<HashMap<String, String>>,
    /// When set, the connections screen asks for this connection's password
    /// or SSH key passphrase.
    pub password_prompt: Signal<Option<PasswordPrompt>>,
    /// When set, the connections screen asks the user to trust an unrecognized
    /// SSH host key before the tunneled connect proceeds (trust-on-first-use).
    pub host_key_prompt: Signal<Option<HostKeyPrompt>>,
    /// When set, the connections screen offers to start an interactive Entra
    /// browser sign-in for a Postgres connect that needs one (FRE-44).
    pub entra_prompt: Signal<Option<EntraPrompt>>,
    /// Set when the user tries to close the window with unsaved staged edits:
    /// the close is vetoed and a discard-and-quit confirmation is shown
    /// (FRE-37). Cleared on cancel or once the user discards and quits.
    pub confirm_quit: Signal<bool>,
    /// Live SSH tunnels, one per tunneled open connection. Removing an entry
    /// drops the [`Tunnel`], which shuts the forward down.
    pub tunnels: Signal<HashMap<ConnectionId, Tunnel>>,
    /// Introspected schema per open connection.
    pub schemas: Signal<HashMap<ConnectionId, SchemaLoad>>,
    /// Sidebar/grid UI state per open connection.
    pub tab_ui: Signal<HashMap<ConnectionId, TabUi>>,
    /// Staged (unsaved) edits per connection, keyed by [`TableRef::key`].
    /// FRE-24/25 push changes in via [`Self::stage_cell_edit`], the
    /// `stage_insert_*` family, and [`Self::stage_delete`];
    /// [`Self::save_staged`] applies a table's stage in one transaction.
    pub staged: Signal<HashMap<ConnectionId, HashMap<String, TableStage>>>,
    /// Per-table grid refresh nonce, keyed by (connection,
    /// [`TableRef::key`]). Lifted out of the grid component so a successful
    /// save can force a refetch; the grid's Refresh button bumps it too.
    /// (Signal subscription is per-signal, so any write here wakes every
    /// mounted grid's readers — the grid therefore reads its own entry
    /// through a PartialEq-gated memo, and only the bumped table's grid
    /// actually resets/refetches; FRE-129.)
    pub grid_refresh: Signal<HashMap<(ConnectionId, String), u64>>,
    /// Two-step unsaved-changes guard. Navigating away from staged edits
    /// (selecting another table, switching panes, closing the tab) does not
    /// happen immediately: the first attempt parks the intent here and the
    /// grid's Save bar shows a blocking notice; repeating the *same* action
    /// at least [`NAV_CONFIRM_MIN_DELAY`] later (double-click protection)
    /// discards the affected stage(s) and proceeds. Any other action —
    /// saving, discarding, staging more edits, or a different navigation —
    /// replaces or clears the parked intent. While a save is in flight the
    /// guarded navigations no-op entirely: discarding then would race the
    /// running transaction.
    ///
    /// Closing the OS window with staged edits is guarded separately by
    /// [`Self::any_dirty`] + [`Self::confirm_quit`] (FRE-37), which raises a
    /// discard-and-quit confirmation rather than losing the edits silently.
    pub nav_guard: Signal<Option<PendingNav>>,
    /// Latest free-form SQL result per connection.
    pub sql_runs: Signal<HashMap<ConnectionId, SqlRun>>,
    /// Scripts containing writes, held here until the user confirms (or
    /// dismisses) the write-confirmation banner.
    pub pending_sql: Signal<HashMap<ConnectionId, PendingSql>>,
    /// Staged saves waiting on the FRE-111 confirmation, one per table, each
    /// holding the exact change list the prompt named.
    ///
    /// Only populated for connections marked [`WriteProtection::Confirm`];
    /// an unmarked connection saves straight through as before. The staged
    /// changes themselves stay in [`Self::staged`] — this holds only a copy,
    /// so dismissing loses nothing, and the copy is what makes a confirmation
    /// stale once the stage moves on (see `take_pending_save`).
    pub pending_saves: Signal<HashMap<(ConnectionId, String), Vec<StagedChange>>>,
    /// Per-connection accent colour (FRE-111), seeded from the saved entry at
    /// connect time.
    ///
    /// Kept beside the registry rather than inside it because a colour warns
    /// and never enforces: nothing in `db/` should be able to read it and act
    /// on it. Its enforcing counterpart, [`WriteProtection`], lives on
    /// [`Connection`] for the opposite reason.
    pub connection_colors: Signal<HashMap<ConnectionId, ConnectionColor>>,
    /// Handle of the in-flight run per connection, kept so the Cancel
    /// button can abort it. Entries are removed when a run completes.
    pub sql_tasks: Signal<HashMap<ConnectionId, Task>>,
    /// Stale-run guard: each started run gets the next generation number,
    /// and a completing task only writes its result while its generation is
    /// still current.
    pub sql_generations: Signal<HashMap<ConnectionId, u64>>,
    /// Persisted query-history store, opened in the background at startup.
    /// `None` while opening or when opening failed (see
    /// [`Self::history_error`]) — history is best-effort and the app works
    /// without it.
    pub history: Signal<Option<HistoryStore>>,
    /// Why the history store is unavailable, shown in the history panel.
    pub history_error: Signal<Option<String>>,
    /// A failed history write for an otherwise-successful run (FRE-72).
    /// Shown as a dismissible banner above the (still readable) history
    /// list; cleared by the next record attempt that reaches the store.
    pub history_record_error: Signal<Option<String>>,
    /// Bumped whenever a run lands in (or is cleared from) the history
    /// store, so open history panels re-query.
    pub history_nonce: Signal<u64>,
    /// UI mirror of the store's persisted recording flag.
    pub history_recording: Signal<bool>,
    /// The persisted theme choice (System / Light / Dark). The toggle cycles
    /// it; [`Self::set_theme`] persists it to settings.toml.
    pub theme: Signal<Theme>,
    /// Resolved dark/light, driving the root `.dark` class. Derived from
    /// `theme` and — for `System` — a one-time startup read of the OS
    /// preference. Root-scoped: written from the startup detection task.
    pub dark: Signal<bool>,
    /// Whether the schema sidebar lists the objects a backend declared
    /// internal (FRE-88). Persisted by
    /// [`Self::set_show_internal_objects`]; global rather than
    /// per-connection, since it expresses what the user wants to look at
    /// rather than anything about one database.
    pub show_internal_objects: Signal<bool>,
    /// A saved-connection edit awaiting the connect that confirms it
    /// (FRE-75). Root-scoped: the Entra sign-in card resolves it from a
    /// background task after the form has closed.
    pub pending_edit: Signal<Option<PendingEdit>>,
    /// Latest export progress per connection and pane. Root-scoped: written
    /// from the `spawn_forever` export task.
    pub export_status: Signal<HashMap<(ConnectionId, ExportPane), ExportStatus>>,
    /// Monotonic id per export slot; a finishing task only records its
    /// outcome while it is still the slot's latest export, so a slow export
    /// can't overwrite the status of one started after it (FRE-73).
    pub export_generations: Signal<HashMap<(ConnectionId, ExportPane), u64>>,
    /// Pending foreign-key focus per connection (FRE-29): a target table plus
    /// the filter to seed. Set by [`Self::navigate_fk`] / [`Self::navigate_back`]
    /// right before the target grid is selected; the grid consumes the entry
    /// matching its table (on mount, or live for a same-table jump) and clears
    /// it. At most one is pending per connection.
    pub pending_focus: Signal<HashMap<ConnectionId, FocusTarget>>,
    /// Back stack per connection (FRE-29): the views left behind by forward
    /// foreign-key jumps, most recent last. [`Self::navigate_back`] pops one;
    /// a manual table selection clears the stack.
    pub nav_history: Signal<HashMap<ConnectionId, Vec<FocusTarget>>>,
    /// Whether the keyboard-shortcut cheatsheet overlay is showing (FRE-15).
    /// App-global (one overlay for the whole window), toggled by `?` and
    /// dismissed by Escape / a backdrop click / `?` again.
    pub show_cheatsheet: Signal<bool>,
}

impl AppState {
    /// Must be called from a component (signals need the runtime).
    pub fn new() -> Self {
        // A failed config load surfaces on the launch screen and the app
        // starts with an empty list; SavedList remembers the failure and
        // refuses to persist over the unreadable file.
        let (saved, load_error) = match default_config_path() {
            Some(path) => {
                let (list, error) = SavedList::load(&path);
                (list, error.map(|e| e.to_string()))
            }
            None => (
                SavedList::load(Path::new("/nonexistent")).0,
                Some("no config directory found".to_string()),
            ),
        };
        // UI preferences: best-effort load, defaults on any problem (settings
        // are non-critical and never block the app).
        let settings = default_settings_path()
            .map(|path| load_settings(&path))
            .unwrap_or_default();
        let theme = settings.theme;
        let state = Self {
            registry: Signal::new(ConnectionRegistry::default()),
            active: Signal::new(ActiveView::Connections),
            saved: Signal::new(saved),
            open_locators: Signal::new(Vec::new()),
            connecting: Signal::new(Vec::new()),
            connect_requests: Signal::new(HashMap::new()),
            connect_error: Signal::new(load_error),
            session_passwords: Signal::new(HashMap::new()),
            password_prompt: Signal::new(None),
            host_key_prompt: Signal::new(None),
            entra_prompt: Signal::new(None),
            confirm_quit: Signal::new(false),
            tunnels: Signal::new(HashMap::new()),
            schemas: Signal::new(HashMap::new()),
            tab_ui: Signal::new(HashMap::new()),
            // Root-scoped: written from the spawn_forever save task.
            staged: Signal::new_in_scope(HashMap::new(), ScopeId::ROOT),
            grid_refresh: Signal::new_in_scope(HashMap::new(), ScopeId::ROOT),
            nav_guard: Signal::new(None),
            sql_runs: Signal::new(HashMap::new()),
            pending_sql: Signal::new(HashMap::new()),
            pending_saves: Signal::new(HashMap::new()),
            connection_colors: Signal::new(HashMap::new()),
            sql_tasks: Signal::new(HashMap::new()),
            sql_generations: Signal::new(HashMap::new()),
            // Root-scoped: these are written from `spawn_forever` tasks
            // (which run in the root scope), so the component scope that
            // built the state must not own them.
            history: Signal::new_in_scope(None, ScopeId::ROOT),
            history_error: Signal::new_in_scope(None, ScopeId::ROOT),
            history_record_error: Signal::new_in_scope(None, ScopeId::ROOT),
            history_nonce: Signal::new_in_scope(0, ScopeId::ROOT),
            history_recording: Signal::new_in_scope(true, ScopeId::ROOT),
            // Root-scoped: the startup detection task below (a
            // `spawn_forever` running in the root scope) reads it, so a
            // component-scoped signal would trip `__copy_value_hoisted`.
            theme: Signal::new_in_scope(theme, ScopeId::ROOT),
            // Start from the persisted theme assuming a light system default;
            // the startup detection task (below) corrects `System`. Root-
            // scoped: written from that spawn_forever task.
            dark: Signal::new_in_scope(theme.resolve_dark(false), ScopeId::ROOT),
            // Component-scoped, unlike its neighbours: only the sidebar
            // reads it, and its setter writes it before spawning, so no
            // root-scoped task ever touches it. (The completion namespace
            // demotes internal objects unconditionally, so it never reads
            // this at all.)
            show_internal_objects: Signal::new(settings.show_internal_objects),
            // Root-scoped: resolved from the Entra sign-in task, which
            // outlives the form that registered the edit.
            pending_edit: Signal::new_in_scope(None, ScopeId::ROOT),
            // Root-scoped: written from the spawn_forever export task.
            export_status: Signal::new_in_scope(HashMap::new(), ScopeId::ROOT),
            export_generations: Signal::new_in_scope(HashMap::new(), ScopeId::ROOT),
            // FK navigation state is only ever touched from UI event handlers,
            // so component scope is fine (like tab_ui / nav_guard).
            pending_focus: Signal::new(HashMap::new()),
            nav_history: Signal::new(HashMap::new()),
            show_cheatsheet: Signal::new(false),
        };
        // Resolve the OS dark-mode preference once at startup. Reacting to
        // live OS theme changes is out of scope (a startup read suffices);
        // an explicit Light/Dark choice overrides it regardless.
        let mut state_for_theme = state;
        spawn_forever(async move {
            if *state_for_theme.theme.peek() != Theme::System {
                return;
            }
            let prefers_dark = system_prefers_dark().await;
            // Guard against a toggle landing before the read resolved.
            if *state_for_theme.theme.peek() == Theme::System {
                state_for_theme.dark.set(prefers_dark);
            }
        });
        // Open the history store in the background; a failure only disables
        // the history panel, never the app. spawn_forever: the opening must
        // not be tied to whichever component happened to create the state.
        let mut state_for_open = state;
        spawn_forever(async move {
            match HistoryStore::open().await {
                Ok(store) => {
                    let enabled = store.recording_enabled().await.unwrap_or(true);
                    state_for_open.history_recording.set(enabled);
                    state_for_open.history.set(Some(store));
                }
                Err(err) => state_for_open.history_error.set(Some(err)),
            }
        });
        // Session restore is triggered once from the Shell component (see
        // `Shell`), not here: it drives the normal `connect`/`connect_postgres`
        // flow, which writes the core connection signals (registry,
        // open_locators, active, tab_ui, …). Those are owned by the root App
        // scope, so running restore from a component-scoped task keeps the
        // writes in a live scope — a root `spawn_forever` here would write
        // them from a foreign scope (the `__copy_value_hoisted` case).
        state
    }

    /// Adds a database file to the saved list (deduped by path) and
    /// persists the list.
    pub fn add_saved(mut self, path: PathBuf) {
        let path = canonical(&path);
        let added = self.saved.write().add(SavedConnection::Sqlite {
            name: tab_title(&path),
            path,
            protection: WriteProtection::Open,
            color: None,
        });
        if added {
            self.persist_saved();
        }
    }

    /// Marks a saved connection's write protection and accent colour
    /// (FRE-111), persists the list, and re-marks any tab already open on it
    /// so the change takes effect now rather than on the next reconnect.
    ///
    /// Marking lives on the connections-list row rather than in the connect
    /// form because it has to reach SQLite entries too, and those have no
    /// edit form — the form only exists for the two server backends.
    ///
    /// `locator` is the entry's stored [`SavedConnection::locator`]. Open tabs
    /// are keyed by the *canonical* [`saved_open_locator`] instead, and for a
    /// SQLite path the two can differ (a symlinked directory, a relative path,
    /// macOS `/tmp` → `/private/tmp`) — `normalize_and_dedup` deliberately
    /// leaves SQLite paths alone. So the open tabs are re-marked through the
    /// canonical form translated from the entry we just wrote, not through
    /// `locator` directly: keying both lookups the same way would persist the
    /// marking while leaving the open tab still accepting writes.
    pub fn set_saved_marking(
        mut self,
        locator: &str,
        protection: WriteProtection,
        color: Option<ConnectionColor>,
    ) {
        let changed = {
            let mut saved = self.saved.write();
            saved.set_marking(locator, protection, color)
        };
        if !changed {
            return;
        }
        self.persist_saved();
        let open_locator = self
            .saved
            .read()
            .entries()
            .iter()
            .find(|saved| saved.locator() == locator)
            .map(saved_open_locator);
        if let Some(open_locator) = open_locator {
            self.remark_open_connections(&open_locator);
        }
    }

    /// Records that the next successful connect to `new_locator` is an edit
    /// of `old_locator` rather than a new connection (FRE-75). Consumed by
    /// [`Self::save_postgres_if_open`] / [`Self::save_sqlserver_if_open`],
    /// the one place every connect path saves through — including the Entra
    /// sign-in card, which completes long after the form has closed.
    pub fn set_pending_edit(mut self, old_locator: String, new_locator: String) {
        self.pending_edit.set(Some(PendingEdit {
            old_locator,
            new_locator,
        }));
    }

    /// Drops a pending edit (the form was cancelled or the connect failed).
    pub fn clear_pending_edit(mut self) {
        if self.pending_edit.peek().is_some() {
            self.pending_edit.set(None);
        }
    }

    /// Saves a just-opened connection, applying a pending edit when one is
    /// waiting for exactly this locator. Matching on the locator keeps a
    /// stale intent (a failed edit the user abandoned) from rewriting an
    /// unrelated connection.
    fn save_or_apply_edit(mut self, connection: SavedConnection) {
        let pending = self
            .pending_edit
            .peek()
            .clone()
            .filter(|edit| edit.new_locator == connection.locator());
        match pending {
            Some(edit) => {
                self.pending_edit.set(None);
                self.update_saved(edit.old_locator, connection);
            }
            None => {
                let added = self.saved.write().add(connection);
                if added {
                    self.persist_saved();
                }
            }
        }
    }

    /// Applies an edit to a saved connection (FRE-75): overwrites the entry
    /// at `old_locator` — name included, which [`Self::save_server_if_open`]
    /// deliberately never does — and persists.
    ///
    /// When the edit changes host/port/database the normalized locator moves
    /// with it, and that locator keys the keyring account too. The stored
    /// secrets are carried across to the new key so an untouched password
    /// keeps working, then dropped from the old one; a secret already stored
    /// under the new locator (the connect that just succeeded wrote one) wins.
    pub fn update_saved(mut self, old_locator: String, connection: SavedConnection) {
        let new_locator = connection.locator().to_string();
        let updated = self.saved.write().update(&old_locator, connection);
        if updated {
            self.persist_saved();
        }
        if new_locator != old_locator {
            spawn_forever(async move {
                for (old, new) in [
                    (old_locator.clone(), new_locator.clone()),
                    (ssh_secret_key(&old_locator), ssh_secret_key(&new_locator)),
                    (
                        entra_secret_key(&old_locator),
                        entra_secret_key(&new_locator),
                    ),
                ] {
                    migrate_secret(old, new).await;
                }
            });
        }
    }

    /// Removes a saved connection (open tabs are unaffected) and persists.
    /// Postgres and SQL Server entries also drop their keyring credentials
    /// (database password, SSH key passphrase, and cached Entra refresh
    /// token; deleting a missing entry is a no-op).
    pub fn remove_saved(mut self, locator: &str) {
        let removed = self.saved.write().remove(locator);
        if let Some(entry) = removed {
            if let SavedConnection::Postgres { url, .. } | SavedConnection::SqlServer { url, .. } =
                entry
            {
                // Best-effort, off-thread: a missing keyring just means
                // nothing was stored.
                spawn_forever(async move {
                    let _ = crate::secrets::delete_password_async(url.clone()).await;
                    let _ = crate::secrets::delete_password_async(ssh_secret_key(&url)).await;
                    let _ = crate::secrets::delete_password_async(entra_secret_key(&url)).await;
                });
            }
            self.persist_saved();
        }
    }

    fn persist_saved(mut self) {
        let Some(config) = default_config_path() else {
            self.connect_error
                .set(Some("no config directory found".to_string()));
            return;
        };
        let result = self.saved.read().persist(&config);
        if let Err(err) = result {
            self.connect_error.set(Some(err.to_string()));
        }
    }

    /// Opens a saved SQLite connection in a new tab, or focuses the existing
    /// tab when the same file is already open.
    pub async fn connect(mut self, path: PathBuf) {
        self.connect_error.set(None);
        let path = canonical(&path);
        let locator = path.display().to_string();
        if self.focus_or_reserve(&locator) {
            return;
        }
        let result = DbPool::open_sqlite(&path).await;
        self.finish_connect(locator, tab_title(&path), result, None);
    }

    /// Opens a saved server connection — Postgres or SQL Server, per
    /// `backend` (FRE-139). With a tunnel configured, the tunnel opens first
    /// (its failures surface as "SSH tunnel: …", distinct from database
    /// errors) and the database connects through the forwarded port; for SQL
    /// Server, TLS keeps validating the server's real hostname (see
    /// [`crate::db::open_mssql_with`]). Entra auth acquires a token silently
    /// or parks the sign-in card. Password auth uses the session password when
    /// one is known, then the OS keyring; otherwise it tries without and falls
    /// back to a password prompt on authentication failure (so trust-auth
    /// servers connect silently).
    pub async fn connect_server(
        mut self,
        backend: ServerBackend,
        url: String,
        name: String,
        tunnel: Option<TunnelConfig>,
        auth: PgAuth,
    ) {
        self.connect_error.set(None);
        if self.focus_or_reserve(&url) {
            // Already open (or reserved): no connect runs, so the save below
            // never fires. An edit still has to land — save_server_if_open
            // no-ops unless the locator is genuinely open (FRE-75).
            self.save_server_if_open(backend, &url, &name, tunnel.clone(), auth.clone());
            return;
        }
        let Some((connect_url, live_tunnel)) =
            self.open_tunnel(&url, &name, &tunnel, &auth, backend).await
        else {
            return; // failure already surfaced (error or passphrase/host-key prompt)
        };
        let tls_host = backend.tls_host(&url, tunnel.is_some());
        match auth {
            PgAuth::Entra(entra) => {
                let pending = EntraPrompt {
                    url,
                    name,
                    tunnel,
                    entra,
                    backend: backend.kind,
                };
                self.connect_server_entra(backend, pending, connect_url, tls_host, live_tunnel)
                    .await;
                return;
            }
            PgAuth::Password => {}
        }
        // Session memory first, then the OS keyring. The keyring call runs
        // off-thread (a locked wallet can block on a user dialog) and only
        // after the session read guard is dropped; errors mean "no keyring"
        // and fall through to the prompt flow.
        self.set_step(&url, ConnectStep::Credentials);
        let mut session_password = self.session_passwords.read().get(&url).cloned();
        if session_password.is_none() {
            session_password = crate::secrets::get_password_async(url.clone())
                .await
                .ok()
                .flatten();
        }
        let had_password = session_password.is_some();
        self.set_step(&url, ConnectStep::Opening);
        let credential = match &session_password {
            Some(password) => ServerCredential::Password(password),
            None => ServerCredential::None,
        };
        let result = backend
            .open(&connect_url, credential, tls_host.as_deref())
            .await;
        match result {
            Err(DbError::Connect(msg)) if msg.contains("authentication failed") => {
                self.release_connect(&url);
                if had_password {
                    self.forget_stale_secret(url.clone(), format!("connection failed: {msg}"))
                        .await;
                }
                // live_tunnel drops here; the retry re-opens it.
                self.password_prompt.set(Some(PasswordPrompt {
                    url,
                    name,
                    kind: PromptKind::DbPassword,
                    backend: backend.kind,
                    tunnel,
                    auth: PgAuth::Password,
                }));
            }
            result => {
                self.finish_connect(url.clone(), name.clone(), result, live_tunnel);
                self.save_server_if_open(backend, &url, &name, tunnel, PgAuth::Password);
            }
        }
    }

    /// The Entra branch of the connect: try to acquire a token silently — a
    /// managed identity always can; interactive only via a cached refresh token
    /// (the browser opener errors, so a missing/expired refresh falls through to
    /// the sign-in card rather than opening a window here). The tunnel is
    /// already open; on a silent success we connect, otherwise park the sign-in.
    async fn connect_server_entra(
        mut self,
        backend: ServerBackend,
        pending: EntraPrompt,
        connect_url: String,
        tls_host: Option<String>,
        live_tunnel: Option<Tunnel>,
    ) {
        self.set_step(&pending.url, ConnectStep::SigningIn);
        let token = acquire_entra(
            &pending.entra,
            backend.entra_resource,
            &pending.url,
            refuse_browser,
        )
        .await;
        match token {
            Ok(token) => {
                self.finish_entra_connect(
                    backend,
                    pending,
                    &connect_url,
                    tls_host,
                    token,
                    live_tunnel,
                )
                .await;
            }
            // Interactive with no usable refresh token: park behind the sign-in
            // card. Drop the tunnel; the sign-in retry re-opens it.
            Err(_) if matches!(pending.entra, EntraAuth::Interactive { .. }) => {
                self.release_connect(&pending.url);
                drop(live_tunnel);
                self.entra_prompt.set(Some(pending));
            }
            Err(err) => {
                drop(live_tunnel);
                self.fail_connect(&pending.url, err.to_string());
            }
        }
    }

    /// Logs in with an acquired Entra token — spliced in as the password for
    /// Postgres, handed to tiberius as its AAD auth method for SQL Server —
    /// and on success caches the refresh token (never the access token) and
    /// saves the connection with its Entra auth mode.
    async fn finish_entra_connect(
        mut self,
        backend: ServerBackend,
        pending: EntraPrompt,
        connect_url: &str,
        tls_host: Option<String>,
        token: azure::AccessToken,
        live_tunnel: Option<Tunnel>,
    ) {
        let EntraPrompt {
            url,
            name,
            tunnel,
            entra,
            ..
        } = pending;
        let result = backend
            .open(
                connect_url,
                ServerCredential::EntraToken(&token.secret),
                tls_host.as_deref(),
            )
            .await;
        let connected = result.is_ok();
        self.finish_connect(url.clone(), name.clone(), result, live_tunnel);
        if connected {
            self.entra_prompt.set(None);
            // Cache the refresh token for silent renewals; best-effort.
            if let Some(refresh) = token.refresh_token {
                let _ = crate::secrets::store_password_async(entra_secret_key(&url), refresh).await;
            }
            self.save_server_if_open(backend, &url, &name, tunnel, PgAuth::Entra(entra));
        }
    }

    /// Resumes an interactive Entra connect from the sign-in card: opens the
    /// browser, waits for the redirect, and connects with the acquired token.
    pub async fn connect_server_with_entra_signin(
        mut self,
        backend: ServerBackend,
        prompt: EntraPrompt,
    ) {
        self.connect_error.set(None);
        self.entra_prompt.set(None);
        if self.focus_or_reserve(&prompt.url) {
            return;
        }
        let Some((connect_url, live_tunnel)) = self
            .open_tunnel(
                &prompt.url,
                &prompt.name,
                &prompt.tunnel,
                &PgAuth::Entra(prompt.entra.clone()),
                backend,
            )
            .await
        else {
            return;
        };
        let tls_host = backend.tls_host(&prompt.url, prompt.tunnel.is_some());
        let token = acquire_entra(
            &prompt.entra,
            backend.entra_resource,
            &prompt.url,
            open_browser,
        )
        .await;
        match token {
            Ok(token) => {
                self.finish_entra_connect(
                    backend,
                    prompt,
                    &connect_url,
                    tls_host,
                    token,
                    live_tunnel,
                )
                .await;
            }
            Err(err) => {
                drop(live_tunnel);
                self.fail_connect(&prompt.url, err.to_string());
                // Re-raise the card so the user can retry the sign-in in place.
                self.entra_prompt.set(Some(prompt));
            }
        }
    }

    /// Completes the password prompt: connects with the entered password
    /// (through the tunnel when one is configured). On success the password
    /// always lives in session memory; with `remember` it is also stored in
    /// the OS keyring (silently staying session-only when no keyring is
    /// available).
    pub async fn connect_server_with_password(
        mut self,
        backend: ServerBackend,
        url: String,
        name: String,
        password: String,
        remember: bool,
        tunnel: Option<TunnelConfig>,
    ) {
        self.connect_error.set(None);
        // The prompt replaces the reservation made by connect_server, so
        // re-reserve here.
        if self.focus_or_reserve(&url) {
            // Already open (or reserved): no connect runs, so the save
            // below never fires. An edit still has to land —
            // save_server_if_open no-ops unless the locator is genuinely open
            // (FRE-75).
            self.save_server_if_open(backend, &url, &name, tunnel.clone(), PgAuth::Password);
            return;
        }
        let Some((connect_url, live_tunnel)) = self
            .open_tunnel(&url, &name, &tunnel, &PgAuth::Password, backend)
            .await
        else {
            return;
        };
        let tls_host = backend.tls_host(&url, tunnel.is_some());
        let result = backend
            .open(
                &connect_url,
                ServerCredential::Password(&password),
                tls_host.as_deref(),
            )
            .await;
        if result.is_ok() {
            if remember {
                // Off-thread; surface a non-fatal notice when the user asked
                // to remember but the keyring store failed.
                let store =
                    crate::secrets::store_password_async(url.clone(), password.clone()).await;
                if store.is_err() {
                    self.connect_error.set(Some(
                        "connected, but the password could not be stored in the system \
                         keyring — it is remembered for this session only"
                            .to_string(),
                    ));
                }
            }
            self.session_passwords.write().insert(url.clone(), password);
            self.password_prompt.set(None);
        }
        self.finish_connect(url.clone(), name.clone(), result, live_tunnel);
        self.save_server_if_open(backend, &url, &name, tunnel, PgAuth::Password);
    }

    /// Completes the SSH-passphrase prompt: remembers the passphrase for the
    /// session and re-runs the parked connect (which now finds it), on the
    /// backend and auth mode the prompt carried. Keyring persistence happens
    /// only after the connect succeeded, so a mistyped passphrase is never
    /// stored.
    ///
    /// Takes the whole prompt rather than its parts because that is what it
    /// completes — and an SSH-passphrase prompt always carries the tunnel
    /// config to resume; one without is a no-op.
    pub async fn connect_server_with_ssh_passphrase(
        mut self,
        prompt: PasswordPrompt,
        passphrase: String,
        remember: bool,
    ) {
        let PasswordPrompt {
            url,
            name,
            backend,
            tunnel,
            auth,
            ..
        } = prompt;
        let Some(tunnel) = tunnel else { return };
        self.stash_ssh_passphrase(&url, passphrase);
        self.password_prompt.set(None);
        self.connect_server(
            ServerBackend::of(backend),
            url.clone(),
            name,
            Some(tunnel),
            auth,
        )
        .await;
        let connected = self.open_locators.read().iter().any(|(_, l)| *l == url);
        if remember && connected {
            self.persist_ssh_passphrase(&url).await;
        }
    }

    /// A successful server connect always joins the saved list (add is a
    /// no-op when URL, tunnel, and auth are already saved, and updates the
    /// tunnel/auth of an existing entry otherwise). This keeps the "connect
    /// first, save on success" contract for every path — including the ones
    /// that went through a prompt or the Entra sign-in card rather than the
    /// form's direct path — which is why the forms themselves never save.
    fn save_server_if_open(
        self,
        backend: ServerBackend,
        url: &str,
        name: &str,
        tunnel: Option<TunnelConfig>,
        auth: PgAuth,
    ) {
        let is_open = self.open_locators.read().iter().any(|(_, l)| l == url);
        if is_open {
            self.save_or_apply_edit(backend.saved(name, url, tunnel, auth));
        }
    }

    /// Drops a secret that just failed from everywhere it is remembered — this
    /// session and the OS keyring — and surfaces `message`, so the retry prompt
    /// starts from a clean slate instead of silently re-offering the credential
    /// that was rejected. `key` is the connection URL for a database password,
    /// [`ssh_secret_key`] for an SSH key passphrase.
    async fn forget_stale_secret(mut self, key: String, message: String) {
        // The write guard is a statement temporary: nothing is held across the
        // keyring await below.
        self.session_passwords.write().remove(&key);
        let _ = crate::secrets::delete_password_async(key).await;
        self.connect_error.set(Some(message));
    }

    /// Completes the host-key trust prompt: records the offered key in
    /// hubro's known_hosts store, then re-runs the connect (which now finds
    /// the host trusted). A failure to persist surfaces as a connect error.
    pub async fn trust_host_and_connect(mut self, prompt: HostKeyPrompt) {
        self.host_key_prompt.set(None);
        let Some(write_path) = crate::tunnel::app_known_hosts_path() else {
            self.connect_error.set(Some(
                "SSH tunnel: no config directory for known_hosts".to_string(),
            ));
            return;
        };
        if let Err(err) = crate::tunnel::trust_host_key(
            &prompt.info.host,
            prompt.info.port,
            &prompt.info.key_openssh,
            &write_path,
        ) {
            self.connect_error.set(Some(err.to_string()));
            return;
        }
        self.connect_server(
            ServerBackend::of(prompt.backend),
            prompt.url,
            prompt.name,
            Some(prompt.tunnel),
            prompt.auth,
        )
        .await;
    }

    /// Puts an SSH key passphrase into session memory so the next tunnel
    /// open for `url` finds it.
    pub fn stash_ssh_passphrase(mut self, url: &str, passphrase: String) {
        self.session_passwords
            .write()
            .insert(ssh_secret_key(url), passphrase);
    }

    /// Stores the session passphrase for `url` in the OS keyring under the
    /// `#ssh` key, surfacing a non-fatal notice when the keyring is
    /// unavailable. Call after a successful tunneled connect.
    pub async fn persist_ssh_passphrase(mut self, url: &str) {
        let key = ssh_secret_key(url);
        let passphrase = self.session_passwords.read().get(&key).cloned();
        let Some(passphrase) = passphrase else {
            return;
        };
        if crate::secrets::store_password_async(key, passphrase)
            .await
            .is_err()
        {
            self.connect_error.set(Some(
                "connected, but the SSH key passphrase could not be stored in the system \
                 keyring — it is remembered for this session only"
                    .to_string(),
            ));
        }
    }

    /// Opens the SSH tunnel when one is configured, returning the URL the
    /// database should actually connect to (host/port rewritten to the
    /// forwarded local port — the saved URL stays the logical one) plus the
    /// live tunnel. `backend` routes the URL helpers (Postgres vs SQL Server
    /// URL shapes) and is carried into any prompt this raises, so the retry
    /// resumes the right connect flow. `None` means the attempt already ended:
    /// the reservation was released and either an error was surfaced or the
    /// passphrase/host-key prompt was raised.
    async fn open_tunnel(
        mut self,
        url: &str,
        name: &str,
        tunnel: &Option<TunnelConfig>,
        auth: &PgAuth,
        backend: ServerBackend,
    ) -> Option<(String, Option<Tunnel>)> {
        let Some(config) = tunnel else {
            return Some((url.to_string(), None));
        };
        self.set_step(url, ConnectStep::Tunnel);
        // The passphrase flows like the database password: session memory,
        // then keyring (off-thread, guard dropped before the await), then a
        // prompt. Only key-file auth can need one.
        let secret_key = ssh_secret_key(url);
        let mut passphrase = None;
        if matches!(config.auth, TunnelAuth::KeyFile { .. }) {
            passphrase = self.session_passwords.read().get(&secret_key).cloned();
            if passphrase.is_none() {
                passphrase = crate::secrets::get_password_async(secret_key.clone())
                    .await
                    .ok()
                    .flatten();
            }
        }
        let had_passphrase = passphrase.is_some();
        let target = match (backend.url_target)(url) {
            Ok(target) => target,
            Err(err) => {
                self.fail_connect(url, err.to_string());
                return None;
            }
        };
        let known_hosts = crate::tunnel::default_known_hosts_read();
        match Tunnel::open(config.clone(), passphrase, target.0, target.1, &known_hosts).await {
            Ok(live) => match (backend.via_local_port)(url, live.local_port()) {
                Ok(rewritten) => Some((rewritten, Some(live))),
                Err(err) => {
                    self.fail_connect(url, err.to_string());
                    None
                }
            },
            Err(err @ TunnelError::NeedsPassphrase(_)) => {
                self.release_connect(url);
                if had_passphrase {
                    // Stored passphrase is stale; drop it everywhere and
                    // re-ask.
                    self.forget_stale_secret(secret_key, err.to_string()).await;
                }
                self.password_prompt.set(Some(PasswordPrompt {
                    url: url.to_string(),
                    name: name.to_string(),
                    kind: PromptKind::SshPassphrase,
                    backend: backend.kind,
                    tunnel: Some(config.clone()),
                    auth: auth.clone(),
                }));
                None
            }
            // First contact: park the connect behind a trust-on-first-use
            // prompt instead of failing. Trusting persists the key and retries.
            Err(TunnelError::HostKeyUnknown(info)) => {
                self.release_connect(url);
                self.host_key_prompt.set(Some(HostKeyPrompt {
                    url: url.to_string(),
                    name: name.to_string(),
                    tunnel: config.clone(),
                    info,
                    auth: auth.clone(),
                    backend: backend.kind,
                }));
                None
            }
            // A changed key is a possible MITM: refuse hard, never offer to
            // trust it. The user must resolve it out-of-band (remove the stale
            // known_hosts entry) before reconnecting.
            Err(err @ TunnelError::HostKeyChanged(_)) => {
                self.fail_connect(url, err.to_string());
                None
            }
            Err(err) => {
                self.fail_connect(url, err.to_string());
                None
            }
        }
    }

    /// Releases a connect reservation and surfaces its error.
    fn fail_connect(mut self, locator: &str, message: String) {
        self.release_connect(locator);
        self.connect_error.set(Some(message));
    }

    /// Starts a connect for a row in the connections list. `focus` is false
    /// for a shift-click, which opens the tab in the background.
    ///
    /// The connect runs on a **root** task, not the caller's: with connects
    /// running in parallel, the first one to finish switches to its tab and
    /// unmounts the connections screen, which would take every sibling
    /// connect's task down with it (the same trap [`Self::load_schema`]
    /// documents). Keeping the handle is also what makes cancelling possible.
    pub fn start_connect(
        mut self,
        locator: String,
        name: String,
        backend: BackendKind,
        tunnel: Option<TunnelConfig>,
        auth: PgAuth,
        focus: bool,
    ) {
        // SQLite reserves under the canonicalized path, so key on that or the
        // row would never match its own progress.
        let key = connect_key(&locator, backend);
        // A second click while the first is still in flight would otherwise
        // overwrite the task handle and strand the running connect.
        if self.connect_requests.read().contains_key(&key) {
            return;
        }
        let task = spawn_forever(async move {
            match backend {
                BackendKind::Postgres | BackendKind::SqlServer => {
                    self.connect_server(ServerBackend::of(backend), locator, name, tunnel, auth)
                        .await;
                }
                BackendKind::Sqlite => self.connect(PathBuf::from(locator)).await,
            }
        });
        self.connect_requests
            .write()
            .insert(key, ConnectRequest { task, focus });
    }

    /// Aborts a connect started from the list. Dropping the task mid-await
    /// unwinds everything it owns — a half-open tunnel included — so there is
    /// nothing else to tear down.
    ///
    /// One step is not interruptible: the keyring read runs on
    /// `spawn_blocking` (see [`crate::secrets`]), and dropping the future
    /// only detaches that thread. Cancelling during `Credentials` therefore
    /// frees the row immediately but leaves a wallet-unlock dialog on screen
    /// until the user answers it.
    pub fn cancel_connect(mut self, locator: &str) {
        if let Some(request) = self.connect_requests.write().remove(locator) {
            request.task.cancel();
        }
        self.connecting.write().retain(|c| c.locator != locator);
    }

    /// Clears a connect's in-flight state. Returns whether the tab it
    /// produced should be focused — false only when a shift-click asked for
    /// the background. Connects with no request (started from a form, or
    /// resumed after a password prompt) focus as before.
    fn release_connect(mut self, locator: &str) -> bool {
        self.connecting.write().retain(|c| c.locator != locator);
        let request = self.connect_requests.write().remove(locator);
        request.is_none_or(|r| r.focus)
    }

    /// Advances the step shown on a connecting row. A no-op once the connect
    /// has finished or been cancelled.
    fn set_step(mut self, locator: &str, step: ConnectStep) {
        if let Some(entry) = self
            .connecting
            .write()
            .iter_mut()
            .find(|c| c.locator == locator)
        {
            entry.step = step;
        }
    }

    /// Focuses the tab if the locator is already open, or reserves it for a
    /// new connect. Returns true when the caller should stop (already open
    /// or connect already in flight). The write borrow is scoped — nothing
    /// spans a later await.
    fn focus_or_reserve(mut self, locator: &str) -> bool {
        let already_open = self
            .open_locators
            .read()
            .iter()
            .find(|(_, l)| l == locator)
            .map(|(id, _)| *id);
        if let Some(id) = already_open {
            // Honours a shift-click here too: re-clicking an open connection
            // in the background should not yank the view to it.
            if self.release_connect(locator) {
                self.active.set(ActiveView::Connection(id));
            }
            return true;
        }
        {
            let mut connecting = self.connecting.write();
            if connecting.iter().any(|c| c.locator == locator) {
                drop(connecting);
                // Someone else's connect owns this locator — a form submit,
                // which reserves without a request. Drop the request
                // `start_connect` just filed for this dead-on-arrival task,
                // or the row would offer a Cancel wired to it: cancelling
                // would clear the row while the real connect ran on, and the
                // next click would start a second one.
                //
                // Only ours: a row-started connect to the same locator may
                // already own the request, and stealing it would cost that
                // one its Cancel button and its background-open intent.
                let mine = dioxus::core::Runtime::current().current_task();
                if let Some(mine) = mine {
                    let mut requests = self.connect_requests.write();
                    if requests.get(locator).is_some_and(|r| r.task == mine) {
                        requests.remove(locator);
                    }
                }
                return true;
            }
            connecting.push(Connecting {
                locator: locator.to_string(),
                step: ConnectStep::Opening,
                visible: false,
            });
        }
        // Reveal the row's progress only once the connect has run long
        // enough to be worth reporting; opening a local SQLite file beats
        // this timer and shows nothing at all.
        let locator = locator.to_string();
        spawn_forever(async move {
            tokio::time::sleep(SPINNER_DELAY).await;
            // No borrow is held across the await above.
            if let Some(entry) = self
                .connecting
                .write()
                .iter_mut()
                .find(|c| c.locator == locator)
            {
                entry.visible = true;
            }
        });
        false
    }

    /// Releases the reservation and either opens the tab (keeping the
    /// tunnel, when there is one, alive for the connection's lifetime) or
    /// surfaces the error (dropping the tunnel).
    fn finish_connect(
        mut self,
        locator: String,
        name: String,
        result: Result<DbPool, DbError>,
        tunnel: Option<Tunnel>,
    ) {
        let focus = self.release_connect(&locator);
        match result {
            Ok(pool) => {
                // The marking (FRE-111) is read here rather than threaded
                // through each connect_* signature: every path — first
                // connect, reconnect, session restore — funnels through this
                // one place, so a new connect path cannot forget it.
                let marking = self.saved_marking(&locator);
                let id = self.registry.write().insert(name, pool, marking.0);
                if let Some(color) = marking.1 {
                    self.connection_colors.write().insert(id, color);
                }
                if let Some(tunnel) = tunnel {
                    self.tunnels.write().insert(id, tunnel);
                }
                self.open_locators.write().push((id, locator));
                if focus {
                    self.active.set(ActiveView::Connection(id));
                }
                // Runs either way: a background tab should be ready to use
                // the moment the user switches to it.
                self.load_schema(id);
            }
            Err(err) => {
                drop(tunnel); // a tunnel without its database is useless
                self.connect_error.set(Some(err.to_string()));
            }
        }
    }

    /// The write protection and accent colour saved for `locator` (FRE-111),
    /// or the defaults when the connection was opened ad hoc rather than from
    /// the saved list.
    fn saved_marking(&self, locator: &str) -> (WriteProtection, Option<ConnectionColor>) {
        self.saved
            .read()
            .entries()
            .iter()
            .find(|saved| saved_open_locator(saved) == locator)
            .map(|saved| (saved.protection(), saved.color()))
            .unwrap_or_default()
    }

    /// This connection's *effective* capabilities resolved for one object —
    /// the backend's (FRE-87) narrowed by the user's marking (FRE-111).
    /// `None` when the connection is gone.
    ///
    /// Every UI gate reads through here so the disabled button, the refused
    /// script and the rejected save all state one reason from one resolution.
    pub fn table_access(&self, id: ConnectionId, table: &TableMeta) -> Option<TableAccess> {
        self.registry.read().get(id).map(|c| c.access(table))
    }

    /// This connection's effective capabilities, ignoring any single object.
    pub fn connection_caps(&self, id: ConnectionId) -> Option<Capabilities> {
        self.registry.read().get(id).map(Connection::capabilities)
    }

    /// Whether writes through this connection must be confirmed first
    /// (FRE-111). False for a connection that is gone.
    pub fn confirms_writes(&self, id: ConnectionId) -> bool {
        self.registry
            .read()
            .get(id)
            .is_some_and(Connection::confirms_writes)
    }

    /// Whether the *user's marking* — rather than the backend — is what makes
    /// this connection unwritable (FRE-111), so the UI can name the right
    /// culprit. False when the engine refuses writes on its own: then the
    /// marking changed nothing and mustn't claim the credit.
    pub fn marked_read_only(&self, id: ConnectionId) -> bool {
        self.registry.read().get(id).is_some_and(|connection| {
            connection.protection == WriteProtection::ReadOnly
                && connection.pool.backend_capabilities().mutate
        })
    }

    /// This connection's accent colour, if the user set one (FRE-111).
    pub fn connection_color(&self, id: ConnectionId) -> Option<ConnectionColor> {
        self.connection_colors.read().get(&id).copied()
    }

    /// Re-marks every open tab on `locator` after its saved entry is edited
    /// (FRE-111), so an open connection starts obeying a new protection
    /// immediately instead of on the next reconnect — the opposite would
    /// leave the tab you just protected still writable.
    fn remark_open_connections(&mut self, locator: &str) {
        let (protection, color) = self.saved_marking(locator);
        let ids: Vec<ConnectionId> = self
            .open_locators
            .read()
            .iter()
            .filter(|(_, open)| open == locator)
            .map(|(id, _)| *id)
            .collect();
        for id in ids {
            self.registry.write().set_protection(id, protection);
            match color {
                Some(color) => {
                    self.connection_colors.write().insert(id, color);
                }
                None => {
                    self.connection_colors.write().remove(&id);
                }
            }
        }
    }

    /// Introspects (or re-introspects) one connection's schema in the
    /// background. The pool is cloned out of the registry before the await;
    /// no signal borrow is held across it.
    pub fn load_schema(mut self, id: ConnectionId) {
        let Some(pool) = self.registry.read().get(id).map(|c| c.pool.clone()) else {
            return;
        };
        self.schemas.write().insert(id, SchemaLoad::Loading);
        // spawn_forever: a plain spawn would tie the task to the calling
        // component's scope, and connect() runs in the connections screen,
        // which unmounts (cancelling its tasks) the moment the view switches
        // to the new tab.
        spawn_forever(async move {
            let loaded = match pool.introspect().await {
                Ok(tables) => SchemaLoad::Ready(tables),
                Err(err) => SchemaLoad::Failed(err.to_string()),
            };
            // The tab may have been closed while introspecting.
            if self.registry.read().get(id).is_some() {
                self.schemas.write().insert(id, loaded);
            }
        });
    }

    /// Switches a tab between the data browser, the SQL editor, and the
    /// schema view. Guarded: leaving the browser while the selected table
    /// has staged edits takes two attempts (see [`Self::nav_guard`]); the
    /// second discards them. While that table's save is in flight the
    /// switch no-ops.
    pub fn set_pane(mut self, id: ConnectionId, pane: Pane) {
        let (current_pane, selected) = {
            let tab_ui = self.tab_ui.read();
            let ui = tab_ui.get(&id);
            (
                ui.map(|ui| ui.pane).unwrap_or_default(),
                ui.and_then(|ui| ui.selected_table.clone()),
            )
        };
        if current_pane == pane {
            return;
        }
        if current_pane == Pane::Browser {
            if let Some(table) = &selected {
                if self.stage_dirty(id, table) {
                    if self.stage_saving(id, table) {
                        return;
                    }
                    if !self.nav_guard_allows(id, NavAction::SetPane(pane)) {
                        return;
                    }
                    self.discard_staged(id, table);
                }
            }
        }
        self.nav_guard.set(None);
        self.tab_ui.write().entry(id).or_default().pane = pane;
    }

    /// Cycles the currently active connection tab through Data → SQL →
    /// Schema (FRE-15's `Ctrl+E` shortcut, extended for the third pane in
    /// FRE-69). A no-op on the connections screen. Routed through
    /// [`Self::set_pane`], so the unsaved-changes guard still applies when
    /// leaving the browser with staged edits.
    pub fn toggle_active_pane(self) {
        let ActiveView::Connection(id) = *self.active.read() else {
            return;
        };
        let current = self
            .tab_ui
            .read()
            .get(&id)
            .map(|ui| ui.pane)
            .unwrap_or_default();
        // Cycles rather than toggles now that there are three panes (FRE-69).
        let target = match current {
            Pane::Browser => Pane::Sql,
            Pane::Sql => Pane::Schema,
            Pane::Schema => Pane::Browser,
        };
        self.set_pane(id, target);
    }

    /// Whether one tab has the row detail panel docked open (FRE-109).
    pub fn row_detail_open(&self, id: ConnectionId) -> bool {
        self.tab_ui.read().get(&id).is_some_and(|ui| ui.row_detail)
    }

    /// Opens or closes one tab's row detail panel. Unlike a pane switch this
    /// is unguarded: the panel is docked beside the grid rather than replacing
    /// it, so closing it navigates nowhere and can't strand staged edits.
    pub fn set_row_detail(mut self, id: ConnectionId, open: bool) {
        self.tab_ui.write().entry(id).or_default().row_detail = open;
    }

    /// Toggles the active tab's row detail panel (FRE-109's shortcut). A no-op
    /// on the connections screen.
    pub fn toggle_row_detail(self) {
        let ActiveView::Connection(id) = *self.active.read() else {
            return;
        };
        self.set_row_detail(id, !self.row_detail_open(id));
    }

    /// The row detail panel's remembered width for one tab, or `None` for the
    /// default (FRE-109).
    pub fn row_detail_width(&self, id: ConnectionId) -> Option<f64> {
        self.tab_ui
            .read()
            .get(&id)
            .and_then(|ui| ui.row_detail_width)
    }

    /// Remembers the width the panel was dragged to. Written once per drag
    /// (on release), not per frame — the drag itself moves the DOM node.
    pub fn set_row_detail_width(mut self, id: ConnectionId, width: f64) {
        self.tab_ui.write().entry(id).or_default().row_detail_width = Some(width);
    }

    /// Flips the shortcut cheatsheet overlay (FRE-15, the `?` shortcut).
    pub fn toggle_cheatsheet(mut self) {
        let showing = *self.show_cheatsheet.read();
        self.show_cheatsheet.set(!showing);
    }

    /// Closes the shortcut cheatsheet if it is open (FRE-15, Escape). Kept
    /// idempotent so a stray Escape never churns the signal.
    pub fn close_cheatsheet(mut self) {
        if *self.show_cheatsheet.read() {
            self.show_cheatsheet.set(false);
        }
    }

    /// Stores the editor buffer (synced from the webview on change). An
    /// actual text change invalidates any pending write confirmation — the
    /// banner must never run SQL that no longer matches the buffer.
    pub fn set_sql_text(mut self, id: ConnectionId, text: String) {
        let changed = {
            let mut tab_ui = self.tab_ui.write();
            let ui = tab_ui.entry(id).or_default();
            let changed = ui.sql_text != text;
            ui.sql_text = text;
            changed
        };
        if changed {
            self.pending_sql.write().remove(&id);
        }
    }

    /// Runs a free-form SQL script against one connection. Scripts where
    /// any statement can mutate the database (see [`needs_confirmation`])
    /// are not executed yet: they are stashed in [`Self::pending_sql`] and
    /// the editor shows a confirmation banner.
    ///
    /// A statement the connection's capabilities forbid (FRE-87) is refused
    /// here, *before* the confirmation banner — being asked to confirm a
    /// write that can never run is a prompt with no right answer.
    pub fn run_sql(mut self, id: ConnectionId, sql: String) {
        self.pending_sql.write().remove(&id);
        // Effective capabilities: the backend's, narrowed by the user's
        // marking (FRE-111). Reading `pool.backend_capabilities()` here instead would
        // let a script write to a connection marked read-only.
        let (dialect, caps) = match self.registry.read().get(id) {
            Some(connection) => (connection.pool.dialect(), connection.capabilities()),
            None => return, // connection closed underneath the editor
        };
        let statements = split_statements(&sql, dialect);
        if statements.is_empty() {
            return;
        }
        if let Some((statement_index, reason)) = script_refusal(caps, &statements, dialect) {
            // Claim the run slot like a real run does: an in-flight script
            // must not push its results into the refusal or overwrite it.
            self.claim_run_slot(id);
            self.sql_runs.write().insert(
                id,
                SqlRun {
                    statements: Vec::new(),
                    status: RunStatus::Refused {
                        reason: reason.to_string(),
                        statement_index,
                        preview: statement_preview(&statements[statement_index]),
                    },
                },
            );
            return;
        }
        if statements.iter().any(|s| needs_confirmation(s, dialect)) {
            self.pending_sql.write().insert(
                id,
                PendingSql {
                    script: sql,
                    statements,
                },
            );
            return;
        }
        self.execute_script(id, sql, statements);
    }

    /// Confirms the write banner: runs the stashed script.
    pub fn confirm_pending_sql(mut self, id: ConnectionId) {
        let pending = self.pending_sql.write().remove(&id);
        if let Some(pending) = pending {
            self.execute_script(id, pending.script, pending.statements);
        }
    }

    /// Dismisses the write banner without running anything.
    pub fn dismiss_pending_sql(mut self, id: ConnectionId) {
        self.pending_sql.write().remove(&id);
    }

    /// Aborts the in-flight run, keeping the outcomes of the statements
    /// that already finished visible and marking the run cancelled.
    ///
    /// Cancelling only drops the sqlx future — on BOTH backends the
    /// in-flight statement itself is NOT interrupted and still completes
    /// (a cancelled UPDATE still commits):
    /// - SQLite: the statement runs to completion on sqlx's worker thread;
    ///   only the future stops being polled.
    /// - Postgres: returning the pooled connection makes sqlx drain the
    ///   socket until the in-flight query has finished server-side, after
    ///   which the connection goes back to the pool healthy — the server
    ///   never sees a disconnect, so the query is not aborted.
    ///
    /// Either way each cancelled long-running query pins one pool
    /// connection until the statement finishes.
    pub fn cancel_sql(mut self, id: ConnectionId) {
        let task = self.sql_tasks.write().remove(&id);
        let Some(task) = task else { return };
        task.cancel();
        if let Some(run) = self.sql_runs.write().get_mut(&id) {
            if run.status == RunStatus::Running {
                run.status = RunStatus::Cancelled;
            }
        }
    }

    /// Takes ownership of this connection's SQL run slot: cancels any
    /// still-running task and bumps the generation, so a run that completes
    /// later can tell it has been superseded and leaves the new result
    /// alone. Returns the new generation.
    fn claim_run_slot(&mut self, id: ConnectionId) -> u64 {
        let previous = self.sql_tasks.write().remove(&id);
        if let Some(previous) = previous {
            previous.cancel();
        }
        let mut generations = self.sql_generations.write();
        let entry = generations.entry(id).or_insert(0);
        *entry += 1;
        *entry
    }

    /// Executes a split script in the background: reads fetch rows, writes
    /// report affected counts, execution stops at the first error. Each
    /// statement's outcome lands in [`Self::sql_runs`] as it finishes.
    fn execute_script(mut self, id: ConnectionId, script: String, statements: Vec<String>) {
        let Some((pool, caps)) = self
            .registry
            .read()
            .get(id)
            .map(|c| (c.pool.clone(), c.capabilities()))
        else {
            return;
        };
        let generation = self.claim_run_slot(id);
        self.sql_runs.write().insert(
            id,
            SqlRun {
                statements: Vec::new(),
                status: RunStatus::Running,
            },
        );
        // spawn_forever: the run must survive pane/tab switches unmounting
        // the editor component. No signal borrow is held across an await —
        // the pool is cloned out above and every write below is scoped.
        let task = spawn_forever(async move {
            let started = std::time::Instant::now();
            let result = run_script(&pool, caps, &statements, |statement| {
                if self.sql_generation(id) == generation {
                    if let Some(run) = self.sql_runs.write().get_mut(&id) {
                        run.statements.push(SharedStatement::new(statement));
                    }
                }
            })
            .await;
            let elapsed_ms = started.elapsed().as_millis() as u64;
            // History is recorded even when a newer run made this one stale —
            // the script did execute. Cancelled runs never reach this point
            // (the future is dropped), so they are not recorded. Recorded
            // fire-and-forget: a wedged history.db must never delay the
            // status update below (FRE-72).
            let error_text = result.as_ref().err().map(|e| e.error.to_string());
            let success = result.is_ok();
            spawn_forever(async move {
                self.record_history(id, script, success, error_text).await;
            });
            // Stale-run guard: a newer run (or a close) owns the slot now.
            if self.sql_generation(id) != generation {
                return;
            }
            self.sql_tasks.write().remove(&id);
            if let Some(run) = self.sql_runs.write().get_mut(&id) {
                run.status = match result {
                    Ok(()) => RunStatus::Done { elapsed_ms },
                    Err(err) => RunStatus::Failed {
                        error: err.error.to_string(),
                        statement_index: err.statement_index,
                        preview: err.preview,
                        elapsed_ms,
                        rolled_back: err.rolled_back,
                    },
                };
            }
        });
        self.sql_tasks.write().insert(id, task);
    }

    fn sql_generation(self, id: ConnectionId) -> u64 {
        self.sql_generations.read().get(&id).copied().unwrap_or(0)
    }

    /// Best-effort history write for a completed run: never blocks or fails
    /// the run itself. All signal reads are scoped before the await; the
    /// nonce bump afterwards tells open history panels to re-query. A write
    /// failure surfaces in the history panel via [`Self::history_error`]
    /// (and clears again on the next successful write).
    async fn record_history(
        mut self,
        id: ConnectionId,
        script: String,
        success: bool,
        error: Option<String>,
    ) {
        let locator = self
            .open_locators
            .read()
            .iter()
            .find(|(open_id, _)| *open_id == id)
            .map(|(_, locator)| locator.clone());
        let Some(locator) = locator else { return };
        let store = self.history.read().clone();
        let Some(store) = store else { return };
        match store
            .record(&locator, &script, success, error.as_deref())
            .await
        {
            Ok(recorded) => {
                // The store answered, so any shown record failure is stale —
                // whether this run was recorded (true) or recording is
                // switched off (false).
                self.history_record_error.set(None);
                if recorded {
                    let mut nonce = self.history_nonce.write();
                    *nonce += 1;
                }
            }
            Err(err) => {
                self.history_record_error
                    .set(Some(format!("Query ran, but recording it failed: {err}")));
            }
        }
    }

    /// Persists the history opt-out flag (best-effort) and mirrors it into
    /// the UI signal immediately.
    pub fn set_history_recording(mut self, enabled: bool) {
        self.history_recording.set(enabled);
        let store = self.history.read().clone();
        let Some(store) = store else { return };
        spawn_forever(async move {
            let _ = store.set_recording(enabled).await;
        });
    }

    /// Sets the theme: updates the resolved `dark` signal immediately and
    /// persists the choice to settings.toml (best-effort — a write failure
    /// only means the choice won't survive a restart, never an error). For
    /// `System` the resolution re-reads the OS preference in the background.
    pub fn set_theme(mut self, theme: Theme) {
        self.theme.set(theme);
        match theme {
            Theme::Light => self.dark.set(false),
            Theme::Dark => self.dark.set(true),
            Theme::System => {
                let mut state = self;
                spawn_forever(async move {
                    let prefers_dark = system_prefers_dark().await;
                    if *state.theme.peek() == Theme::System {
                        state.dark.set(prefers_dark);
                    }
                });
            }
        }
        // Persist off the UI path; no signal borrow is held across the write.
        // `save_theme` merges into the existing file so the window geometry
        // (FRE-30, same settings.toml) isn't clobbered.
        let Some(path) = default_settings_path() else {
            return;
        };
        spawn_forever(async move {
            let _ = save_theme(&path, theme);
        });
    }

    /// Shows or hides the database's own internal objects in the sidebar
    /// (FRE-88) and persists the choice, best-effort like
    /// [`Self::set_theme`].
    pub fn set_show_internal_objects(mut self, show: bool) {
        self.show_internal_objects.set(show);
        let Some(path) = default_settings_path() else {
            return;
        };
        spawn_forever(async move {
            let _ = save_show_internal_objects(&path, show);
        });
    }

    /// Deletes one connection's history and refreshes open panels.
    pub fn clear_history(self, id: ConnectionId) {
        let locator = self
            .open_locators
            .read()
            .iter()
            .find(|(open_id, _)| *open_id == id)
            .map(|(_, locator)| locator.clone());
        let Some(locator) = locator else { return };
        let store = self.history.read().clone();
        let Some(store) = store else { return };
        let mut nonce_signal = self.history_nonce;
        spawn_forever(async move {
            if store.clear(&locator).await.is_ok() {
                let mut nonce = nonce_signal.write();
                *nonce += 1;
            }
        });
    }

    /// Marks a table as selected in one tab's sidebar. Guarded: switching
    /// away from a table with staged edits takes two attempts (see
    /// [`Self::nav_guard`]); the second discards them and switches. While
    /// that table's save is in flight the switch no-ops.
    ///
    /// A manual selection clears the foreign-key Back stack and any pending
    /// focus — the FK trail only makes sense relative to the jumps that built
    /// it, not to an unrelated sidebar pick.
    pub fn select_table(self, id: ConnectionId, table: &TableRef) {
        self.switch_table(id, table, true);
    }

    /// Switches the selected table, running the unsaved-changes guard. Returns
    /// whether the selection actually changed: a no-op (already selected) or a
    /// parked/blocked guard returns `false`. `clear_fk_nav` drops this
    /// connection's FK Back stack and pending focus on a real switch —
    /// foreign-key jumps and Back pass `false` so they manage that stack
    /// themselves.
    fn switch_table(mut self, id: ConnectionId, table: &TableRef, clear_fk_nav: bool) -> bool {
        let current = self
            .tab_ui
            .read()
            .get(&id)
            .and_then(|ui| ui.selected_table.clone());
        if current.as_ref() == Some(table) {
            return false;
        }
        if let Some(current_table) = &current {
            if self.stage_dirty(id, current_table) {
                if self.stage_saving(id, current_table) {
                    return false;
                }
                if !self.nav_guard_allows(id, NavAction::SelectTable(table.clone())) {
                    return false;
                }
                self.discard_staged(id, current_table);
            }
        }
        self.nav_guard.set(None);
        self.tab_ui.write().entry(id).or_default().selected_table = Some(table.clone());
        if clear_fk_nav {
            self.nav_history.write().remove(&id);
            self.pending_focus.write().remove(&id);
        }
        true
    }

    /// Whether this connection has anywhere to go Back to (a non-empty FK
    /// Back stack). Drives the grid's Back button visibility.
    pub fn can_go_back(&self, id: ConnectionId) -> bool {
        self.nav_history
            .read()
            .get(&id)
            .is_some_and(|stack| !stack.is_empty())
    }

    /// Follows a foreign key from `origin` to the row it references (FRE-29):
    /// resolves the target table, builds the multi-equality filter that pins
    /// the referenced row from `source_row`, records the origin view on the
    /// Back stack, and selects the target (seeding its filter through
    /// [`Self::pending_focus`]).
    ///
    /// A no-op when the jump can't be built — any FK column's source value is
    /// missing or NULL (a NULL foreign key references nothing), or a referenced
    /// column can't be resolved. Guarded like any table switch: leaving a table
    /// with unsaved edits takes the usual second confirming click (a
    /// self-referencing FK only changes the filter, so it is never guarded).
    pub fn navigate_fk(
        mut self,
        id: ConnectionId,
        fk: &ForeignKeyMeta,
        source_row: &HashMap<String, Value>,
        origin: &TableRef,
        origin_filter: Option<Filter>,
    ) {
        let target = TableRef {
            schema: fk.referenced_schema.clone(),
            name: fk.referenced_table.clone(),
        };
        // The target PK is only consulted for FK columns that reference the
        // target's implicit primary key (`referenced_columns[i] == None`).
        let target_pk = self.table_primary_key(id, &target);
        let Some(filter) = build_fk_filter(fk, source_row, &target_pk) else {
            return;
        };
        let focus = FocusTarget {
            table: target.clone(),
            filter: Some(filter),
        };
        let origin_focus = FocusTarget {
            table: origin.clone(),
            filter: origin_filter,
        };
        self.pending_focus.write().insert(id, focus);
        if &target == origin {
            // Self-referencing FK: the grid stays mounted and just refocuses
            // its filter (consumed by its pending-focus effect). Record the
            // origin so Back can restore the prior filter.
            self.nav_history
                .write()
                .entry(id)
                .or_default()
                .push(origin_focus);
        } else if self.switch_table(id, &target, false) {
            self.nav_history
                .write()
                .entry(id)
                .or_default()
                .push(origin_focus);
        }
    }

    /// Returns to the most recent view on the FK Back stack (FRE-29),
    /// restoring its table and filter. A no-op when the stack is empty. The
    /// entry is only popped once the return actually happens, so a staged-edit
    /// guard parking the switch keeps it for the confirming click.
    pub fn navigate_back(mut self, id: ConnectionId) {
        let target = self
            .nav_history
            .read()
            .get(&id)
            .and_then(|stack| stack.last().cloned());
        let Some(target) = target else {
            return;
        };
        let current = self
            .tab_ui
            .read()
            .get(&id)
            .and_then(|ui| ui.selected_table.clone());
        let dest = target.table.clone();
        self.pending_focus.write().insert(id, target);
        // Same-table restore only refocuses the filter (the grid's effect
        // consumes it); otherwise switch, honoring the unsaved-edits guard.
        let restored = current.as_ref() == Some(&dest) || self.switch_table(id, &dest, false);
        if restored {
            if let Some(stack) = self.nav_history.write().get_mut(&id) {
                stack.pop();
            }
        }
    }

    /// Loads one cell's full value on demand — the grid's expand and
    /// truncated-cell editing path (FRE-33). Clones the pool and this table's
    /// metadata out of the signals before the await (no borrow spans it), then
    /// targets the row through its [`RowLocator`]. Returns a user-facing error
    /// string on any failure. Never `spawn`ed itself — callers drive it from a
    /// `use_resource` so the fetch is tied to the open editor/overlay.
    pub async fn load_cell(
        self,
        id: ConnectionId,
        table: TableRef,
        locator: RowLocator,
        column: String,
    ) -> Result<CellFetch, String> {
        let pool = self.registry.read().get(id).map(|c| c.pool.clone());
        let meta = self.schemas.read().get(&id).and_then(|load| match load {
            SchemaLoad::Ready(tables) => tables
                .iter()
                .find(|t| t.name == table.name && t.schema == table.schema)
                .cloned(),
            _ => None,
        });
        let (Some(pool), Some(meta)) = (pool, meta) else {
            return Err("connection or schema no longer available".into());
        };
        // A read path: it only needs the row to be addressable, so it asks
        // for the identity alone rather than a full access it has no business
        // reading a write capability off — a read-only connection, whether by
        // engine (FRE-87) or by marking (FRE-111), still expands cells.
        let Some(identity) = pool.backend_row_identity(&meta) else {
            return Err("this table has no usable row identity".into());
        };
        pool.fetch_cell(&meta, &identity, &locator, &column)
            .await
            .map_err(|e| e.to_string())
    }

    /// DDL for one of a table's objects (FRE-108): the table/view itself, or
    /// one of its indexes. Same shape as [`Self::load_cell`] — the pool and
    /// metadata are cloned out of the signals before the await. No capability
    /// gate: reading a definition is a pure read, so a read-only connection
    /// shows DDL like any other.
    pub async fn load_ddl(
        self,
        id: ConnectionId,
        table: TableRef,
        object: DdlObject,
    ) -> Result<Ddl, String> {
        let pool = self.registry.read().get(id).map(|c| c.pool.clone());
        let meta = self.schemas.read().get(&id).and_then(|load| match load {
            SchemaLoad::Ready(tables) => tables
                .iter()
                .find(|t| t.name == table.name && t.schema == table.schema)
                .cloned(),
            _ => None,
        });
        let (Some(pool), Some(meta)) = (pool, meta) else {
            return Err("connection or schema no longer available".into());
        };
        pool.fetch_ddl(&meta, &object)
            .await
            .map_err(|e| e.to_string())
    }

    /// The primary-key column names of `table` in key order, from the loaded
    /// schema (empty when the schema isn't ready or the table has no PK).
    fn table_primary_key(&self, id: ConnectionId, table: &TableRef) -> Vec<String> {
        match self.schemas.read().get(&id) {
            Some(SchemaLoad::Ready(tables)) => tables
                .iter()
                .find(|t| t.name == table.name && t.schema == table.schema)
                .map(|t| t.primary_key().iter().map(|c| c.name.clone()).collect())
                .unwrap_or_default(),
            _ => Vec::new(),
        }
    }

    /// Stages a cell edit (FRE-24 pushes edits in through this). Edits
    /// coalesce per `(row, column)` — the last staged value wins. Staging
    /// is allowed even while a save is in flight; see [`Self::save_staged`]
    /// for the concurrency contract.
    pub fn stage_cell_edit(
        mut self,
        id: ConnectionId,
        table: &TableRef,
        locator: RowLocator,
        column: &str,
        value: Value,
    ) {
        self.nav_guard.set(None);
        self.staged
            .write()
            .entry(id)
            .or_default()
            .entry(table.key())
            .or_default()
            .set_cell_edit(locator, column, value);
    }

    /// Stages a new all-default pending insert — the "+ New row" affordance
    /// (FRE-25). Columns get concrete values via [`Self::stage_insert_value`].
    pub fn stage_insert_row(mut self, id: ConnectionId, table: &TableRef) {
        self.nav_guard.set(None);
        self.staged
            .write()
            .entry(id)
            .or_default()
            .entry(table.key())
            .or_default()
            .add_insert();
    }

    /// Stages a concrete value for one column of a pending insert (last one
    /// wins, like cell edits). No-ops when the phantom row no longer exists.
    pub fn stage_insert_value(
        mut self,
        id: ConnectionId,
        table: &TableRef,
        insert_id: u64,
        column: &str,
        value: Value,
    ) {
        self.nav_guard.set(None);
        if let Some(stage) = self
            .staged
            .write()
            .get_mut(&id)
            .and_then(|tables| tables.get_mut(&table.key()))
        {
            stage.set_insert_value(insert_id, column, value);
        }
    }

    /// Reverts one column of a pending insert to "database default".
    pub fn clear_insert_value(
        mut self,
        id: ConnectionId,
        table: &TableRef,
        insert_id: u64,
        column: &str,
    ) {
        self.nav_guard.set(None);
        if let Some(stage) = self
            .staged
            .write()
            .get_mut(&id)
            .and_then(|tables| tables.get_mut(&table.key()))
        {
            stage.clear_insert_value(insert_id, column);
        }
    }

    /// Removes a pending insert — "deleting" a phantom row stages nothing,
    /// the row just disappears. A stage this empties is cleaned up (so the
    /// Save bar goes away), except while a save is in flight — its
    /// bookkeeping (`saving`, `last_error`) must survive.
    pub fn remove_pending_insert(mut self, id: ConnectionId, table: &TableRef, insert_id: u64) {
        self.nav_guard.set(None);
        let mut staged = self.staged.write();
        let Some(tables) = staged.get_mut(&id) else {
            return;
        };
        let Some(stage) = tables.get_mut(&table.key()) else {
            return;
        };
        stage.remove_insert(insert_id);
        if stage.is_empty() && !stage.saving {
            tables.remove(&table.key());
            if tables.is_empty() {
                staged.remove(&id);
            }
        }
    }

    /// Stages a row delete (FRE-25 pushes deletes in through this).
    pub fn stage_delete(mut self, id: ConnectionId, table: &TableRef, locator: RowLocator) {
        self.nav_guard.set(None);
        self.staged
            .write()
            .entry(id)
            .or_default()
            .entry(table.key())
            .or_default()
            .mark_delete(locator);
    }

    /// The current stage of one table view, if any (cloned for rendering).
    pub fn table_stage(&self, id: ConnectionId, table: &TableRef) -> Option<TableStage> {
        self.staged
            .read()
            .get(&id)
            .and_then(|tables| tables.get(&table.key()))
            .cloned()
    }

    /// Discards all staged changes of one table view. Refused (no-op) while
    /// that table's save is in flight — the running transaction may still
    /// commit, and "discarded" changes silently landing in the database
    /// would be worse than a briefly stuck Discard button.
    pub fn discard_staged(mut self, id: ConnectionId, table: &TableRef) {
        let mut staged = self.staged.write();
        let Some(tables) = staged.get_mut(&id) else {
            return;
        };
        if tables.get(&table.key()).is_some_and(|stage| stage.saving) {
            return;
        }
        tables.remove(&table.key());
        if tables.is_empty() {
            staged.remove(&id);
        }
        drop(staged);
        // A confirmation parked over changes that no longer exist (FRE-111)
        // would otherwise sit there offering to apply nothing.
        self.pending_saves.write().remove(&(id, table.key()));
        self.nav_guard.set(None);
    }

    /// Whether one table view has pending staged changes.
    fn stage_dirty(&self, id: ConnectionId, table: &TableRef) -> bool {
        self.staged
            .read()
            .get(&id)
            .and_then(|tables| tables.get(&table.key()))
            .is_some_and(|stage| !stage.is_empty())
    }

    /// Whether one table view has a save in flight.
    fn stage_saving(&self, id: ConnectionId, table: &TableRef) -> bool {
        self.staged
            .read()
            .get(&id)
            .and_then(|tables| tables.get(&table.key()))
            .is_some_and(|stage| stage.saving)
    }

    /// Whether any table of the connection has pending staged changes.
    fn any_stage_dirty(&self, id: ConnectionId) -> bool {
        self.staged
            .read()
            .get(&id)
            .is_some_and(|tables| tables.values().any(|stage| !stage.is_empty()))
    }

    /// Whether *any* open connection has pending staged edits anywhere. The
    /// window-close guard (FRE-37) uses this: unlike the per-connection
    /// navigation guards, closing the OS window isn't scoped to one connection.
    pub fn any_dirty(&self) -> bool {
        staged_has_dirty(&self.staged.read())
    }

    /// Whether any table of the connection has a save in flight.
    fn any_stage_saving(&self, id: ConnectionId) -> bool {
        self.staged
            .read()
            .get(&id)
            .is_some_and(|tables| tables.values().any(|stage| stage.saving))
    }

    /// Runs the two-step guard for one navigation attempt. Returns `true`
    /// when the attempt may proceed: the same intent was parked at least
    /// [`NAV_CONFIRM_MIN_DELAY`] ago. Otherwise parks the intent (first
    /// attempt or a different action) or ignores it (identical repeat
    /// inside the double-click floor — the original park time is kept so a
    /// deliberate later repeat still confirms).
    fn nav_guard_allows(mut self, id: ConnectionId, action: NavAction) -> bool {
        let parked = self.nav_guard.read().clone();
        match parked {
            Some(nav) if nav.matches(id, &action) => nav.confirmable(),
            _ => {
                self.nav_guard.set(Some(PendingNav::new(id, action)));
                false
            }
        }
    }

    /// Forces the grid of one table to refetch (used by the grid's Refresh
    /// button and by [`Self::save_staged`] after a successful apply).
    pub fn bump_grid_refresh(mut self, id: ConnectionId, table_key: &str) {
        let mut refresh = self.grid_refresh.write();
        *refresh.entry((id, table_key.to_string())).or_insert(0) += 1;
    }

    /// Applies one table's staged changes in ONE transaction, in the
    /// background. On success the applied changes are removed from the
    /// stage and the grid refetches; on failure the stage stays intact (so
    /// the user can fix or discard) and the Save bar shows which change
    /// failed.
    ///
    /// Concurrency contract (FRE-24/25 rely on this): staging MORE changes
    /// while a save is in flight is allowed — the save snapshots the change
    /// list up front and, on success, removes exactly that snapshot from
    /// the stage ([`TableStage::remove_applied`]), so later edits survive
    /// and keep the Save bar visible. Only a second save and discard are
    /// blocked while `saving` is set.
    /// On a connection marked [`WriteProtection::Confirm`] (FRE-111) the
    /// first call parks the intent in [`Self::pending_saves`] and returns —
    /// the grid then shows a confirmation naming the connection, and
    /// [`Self::confirm_pending_save`] calls back in to do the work. Nothing
    /// is staged, unstaged or sent in the meantime, so dismissing costs the
    /// user nothing.
    pub fn save_staged(mut self, id: ConnectionId, table: &TableRef) {
        let key = (id, table.key());
        // The parked intent is consumed first either way: `save_action`
        // decides whether it still authorizes *these* changes, and a stale
        // one is replaced rather than honoured. Taken before the empty-stage
        // return so an emptied stage can't strand a banner offering to apply
        // nothing.
        let parked = self.pending_saves.write().remove(&key);
        let current = match self.table_stage(id, table) {
            Some(stage) if !stage.is_empty() => stage.changes(),
            _ => return,
        };
        let protection = self.protection_of(id);
        match save_action(protection, parked.as_deref(), &current) {
            SaveAction::Apply => self.apply_staged_now(id, table),
            SaveAction::Park => {
                self.pending_saves.write().insert(key, current);
            }
        }
    }

    /// Confirms the FRE-111 save banner: applies the staged changes.
    ///
    /// Routed through [`Self::save_staged`] rather than applying directly, so
    /// the staleness check runs here too — the Confirm button and a second
    /// Save click must not be able to disagree about what was authorized.
    pub fn confirm_pending_save(self, id: ConnectionId, table: &TableRef) {
        self.save_staged(id, table);
    }

    /// Dismisses the FRE-111 save banner, leaving the staged changes alone.
    pub fn dismiss_pending_save(mut self, id: ConnectionId, table: &TableRef) {
        self.pending_saves.write().remove(&(id, table.key()));
    }

    /// Whether this table's save is waiting on the FRE-111 confirmation.
    pub fn save_awaiting_confirmation(&self, id: ConnectionId, table_key: &str) -> bool {
        self.pending_saves
            .read()
            .contains_key(&(id, table_key.to_string()))
    }

    /// This connection's write protection, defaulting to `Open` for a
    /// connection that is gone.
    fn protection_of(&self, id: ConnectionId) -> WriteProtection {
        self.registry
            .read()
            .get(id)
            .map(|c| c.protection)
            .unwrap_or_default()
    }

    fn apply_staged_now(mut self, id: ConnectionId, table: &TableRef) {
        let table_key = table.key();
        // Snapshot the normalized change list and flip the in-flight flag —
        // one scoped write, nothing spans the await below.
        let changes = {
            let mut staged = self.staged.write();
            let Some(stage) = staged.get_mut(&id).and_then(|t| t.get_mut(&table_key)) else {
                return;
            };
            if stage.saving || stage.is_empty() {
                return;
            }
            stage.saving = true;
            stage.last_error = None;
            stage.changes()
        };
        self.nav_guard.set(None);
        let pool = self.registry.read().get(id).map(|c| c.pool.clone());
        let meta = self.schemas.read().get(&id).and_then(|load| match load {
            SchemaLoad::Ready(tables) => tables
                .iter()
                .find(|t| t.name == table.name && t.schema == table.schema)
                .cloned(),
            _ => None,
        });
        let (Some(pool), Some(meta)) = (pool, meta) else {
            self.fail_save(
                id,
                &table_key,
                "connection or schema no longer available".into(),
            );
            return;
        };
        // The same resolution the grid gated its editors on (FRE-87, and the
        // user's marking from FRE-111); if a stage exists here anyway, the
        // failure states the resolver's reason rather than a second,
        // differently-worded one.
        let Some(access) = self.table_access(id, &meta) else {
            self.fail_save(id, &table_key, "connection no longer available".into());
            return;
        };
        let Some(identity) = access.identity.clone().filter(|_| access.can_mutate()) else {
            self.fail_save(
                id,
                &table_key,
                access
                    .read_only_notice()
                    .unwrap_or("This table has no usable row identity.")
                    .to_string(),
            );
            return;
        };
        // spawn_forever: the save must survive the grid unmounting (e.g. a
        // guarded navigation completing while the apply runs).
        spawn_forever(async move {
            let result = apply_staged(&pool, &access, &meta, &identity, &changes).await;
            match result {
                Ok(_counts) => {
                    {
                        // Remove exactly the snapshotted changes: anything
                        // staged after the snapshot survives (and keeps the
                        // Save bar up) instead of being silently destroyed.
                        let mut staged = self.staged.write();
                        if let Some(tables) = staged.get_mut(&id) {
                            if let Some(stage) = tables.get_mut(&table_key) {
                                stage.remove_applied(&changes);
                                if stage.is_empty() {
                                    tables.remove(&table_key);
                                }
                            }
                            if tables.is_empty() {
                                staged.remove(&id);
                            }
                        }
                    }
                    self.bump_grid_refresh(id, &table_key);
                }
                Err(err) => {
                    // Name the failing change so the user can find it (for a
                    // grouped update: the row and its columns).
                    let message = match (err.change_index, &err.change_summary) {
                        (Some(index), Some(summary)) => format!(
                            "change {} of {} ({summary}) failed: {} — nothing was applied",
                            index + 1,
                            changes.len(),
                            err.message
                        ),
                        // No index: the transaction itself failed to open or
                        // commit, so there is no rollback guarantee to claim.
                        _ => format!(
                            "{} — the batch may or may not have been applied; \
                             refresh to see the current state",
                            err.message
                        ),
                    };
                    self.fail_save(id, &table_key, message);
                }
            }
        });
    }

    /// Streams a live query (the current grid view: filter + sort, no paging)
    /// to `path` in `format`, in a background task. The query re-runs against
    /// the connection so it always reflects committed data; rows are pulled
    /// one at a time and written incrementally (see
    /// [`DbPool::export`](crate::db::DbPool::export)). Progress lands in
    /// [`Self::export_status`]; the UI never blocks.
    pub fn export_query(
        mut self,
        id: ConnectionId,
        sql: String,
        params: Vec<Value>,
        format: ExportFormat,
        path: PathBuf,
    ) {
        let slot = (id, ExportPane::Grid);
        let pool = self.registry.read().get(id).map(|c| c.pool.clone());
        let Some(pool) = pool else {
            self.begin_export(slot);
            self.export_status
                .write()
                .insert(slot, ExportStatus::Failed("connection closed".into()));
            return;
        };
        let generation = self.begin_export(slot);
        // spawn_forever: the export must survive the grid unmounting (a pane
        // or tab switch) — a plain spawn would cancel it mid-write.
        spawn_forever(async move {
            use std::io::Write as _;
            // Stream into a temp file and rename on success, so a mid-stream
            // failure can't clobber an existing file at `path`.
            let tmp = export_temp_path(&path);
            let outcome = async {
                let file = std::fs::File::create(&tmp).map_err(|e| e.to_string())?;
                let mut writer = std::io::BufWriter::new(file);
                let rows = pool
                    .export(&sql, &params, format, &mut writer)
                    .await
                    .map_err(|e| e.to_string())?;
                writer.flush().map_err(|e| e.to_string())?;
                drop(writer);
                std::fs::rename(&tmp, &path).map_err(|e| e.to_string())?;
                Ok::<u64, String>(rows)
            }
            .await;
            if outcome.is_err() {
                let _ = std::fs::remove_file(&tmp);
            }
            self.finish_export(slot, generation, outcome);
        });
    }

    /// Writes an already-materialized [`QueryResult`] (the SQL editor's held
    /// result) to `path` in `format`, in a background task. Shares the row
    /// formatters with [`Self::export_query`]; no database round-trip.
    pub fn export_result(
        self,
        id: ConnectionId,
        result: QueryResult,
        format: ExportFormat,
        path: PathBuf,
    ) {
        let slot = (id, ExportPane::Sql);
        let generation = self.begin_export(slot);
        spawn_forever(async move {
            use std::io::Write as _;
            let tmp = export_temp_path(&path);
            let outcome = (|| {
                let file = std::fs::File::create(&tmp).map_err(|e| e.to_string())?;
                let mut writer = std::io::BufWriter::new(file);
                let rows = write_result(&result, format, &mut writer).map_err(|e| e.to_string())?;
                writer.flush().map_err(|e| e.to_string())?;
                drop(writer);
                std::fs::rename(&tmp, &path).map_err(|e| e.to_string())?;
                Ok::<u64, String>(rows)
            })();
            if outcome.is_err() {
                let _ = std::fs::remove_file(&tmp);
            }
            self.finish_export(slot, generation, outcome);
        });
    }

    /// Marks a slot Running and returns the new export's generation.
    fn begin_export(mut self, slot: (ConnectionId, ExportPane)) -> u64 {
        let generation = {
            let mut generations = self.export_generations.write();
            let entry = generations.entry(slot).or_insert(0);
            *entry += 1;
            *entry
        };
        self.export_status
            .write()
            .insert(slot, ExportStatus::Running);
        generation
    }

    /// Records an export's terminal status — unless a newer export owns the
    /// slot, in which case this outcome is stale and dropped.
    fn finish_export(
        mut self,
        slot: (ConnectionId, ExportPane),
        generation: u64,
        outcome: Result<u64, String>,
    ) {
        let latest = self.export_generations.read().get(&slot).copied();
        if latest != Some(generation) {
            return;
        }
        let status = match outcome {
            Ok(rows) => ExportStatus::Done { rows },
            Err(err) => ExportStatus::Failed(err),
        };
        self.export_status.write().insert(slot, status);
    }

    /// Records a failed save on the stage (kept intact) and re-enables Save.
    fn fail_save(mut self, id: ConnectionId, table_key: &str, message: String) {
        let mut staged = self.staged.write();
        if let Some(stage) = staged.get_mut(&id).and_then(|t| t.get_mut(table_key)) {
            stage.saving = false;
            stage.last_error = Some(message);
        }
    }

    /// Closes a tab: drops it from the registry, closes the pool in the
    /// background, shuts down its SSH tunnel (if any), and leaves the view
    /// somewhere sensible.
    ///
    /// Guarded: closing while ANY table of the connection has staged edits
    /// takes two attempts (see [`Self::nav_guard`]) — closing is the one
    /// navigation that actually destroys them — and no-ops entirely while a
    /// save is in flight. The notice renders in the dirty table's Save bar,
    /// so when another pane or tab is in front the first click can look
    /// inert; the second click still closes. A deliberate simplification,
    /// kept until a global toast/dialog exists. (Closing the OS window is
    /// guarded separately by the discard-and-quit confirmation, FRE-37.)
    pub fn close_connection(mut self, id: ConnectionId) {
        if self.any_stage_dirty(id) {
            if self.any_stage_saving(id) {
                return;
            }
            if !self.nav_guard_allows(id, NavAction::CloseConnection) {
                return;
            }
        }
        self.nav_guard.set(None);
        self.staged.write().remove(&id);
        self.grid_refresh.write().retain(|(conn, _), _| *conn != id);
        let removed = self.registry.write().remove(id);
        if let Some(connection) = removed {
            // spawn_forever so the close isn't cancelled if the calling
            // component unmounts first.
            spawn_forever(async move { connection.pool.close().await });
        }
        // Dropping the Tunnel signals its forward task to shut down.
        self.tunnels.write().remove(&id);
        self.open_locators
            .write()
            .retain(|(open_id, _)| *open_id != id);
        self.schemas.write().remove(&id);
        self.tab_ui.write().remove(&id);
        self.sql_runs.write().remove(&id);
        self.pending_sql.write().remove(&id);
        self.pending_saves
            .write()
            .retain(|(conn, _), _| *conn != id);
        self.connection_colors.write().remove(&id);
        self.export_status
            .write()
            .retain(|(conn, _), _| *conn != id);
        self.export_generations
            .write()
            .retain(|(conn, _), _| *conn != id);
        self.pending_focus.write().remove(&id);
        self.nav_history.write().remove(&id);
        // Abort any in-flight run and drop its bookkeeping; bumping nothing
        // is fine — removing the generation entry makes any still-alive
        // task's generation stale.
        let task = self.sql_tasks.write().remove(&id);
        if let Some(task) = task {
            task.cancel();
        }
        self.sql_generations.write().remove(&id);
        if *self.active.read() == ActiveView::Connection(id) {
            self.active.set(ActiveView::Connections);
        }
    }

    /// Snapshots the current session (FRE-30): the open tabs in tab order,
    /// each with its selected table and pane, plus the active tab's locator.
    /// Cheap and pure — the SQL editor buffer is deliberately not included, so
    /// per-keystroke `tab_ui` changes produce an identical snapshot and the
    /// persistence effect skips the write.
    pub fn current_session(&self) -> Session {
        let open = self.open_locators.read();
        let tab_ui = self.tab_ui.read();
        let mut tabs = Vec::with_capacity(open.len());
        for (id, locator) in open.iter() {
            let (schema, table, pane, row_detail) = match tab_ui.get(id) {
                Some(ui) => (
                    ui.selected_table.as_ref().and_then(|t| t.schema.clone()),
                    ui.selected_table.as_ref().map(|t| t.name.clone()),
                    ui.pane.to_session(),
                    ui.row_detail,
                ),
                None => (None, None, SessionPane::default(), false),
            };
            tabs.push(SessionTab {
                locator: locator.clone(),
                selected_schema: schema,
                selected_table: table,
                pane,
                row_detail,
            });
        }
        let active = match *self.active.read() {
            ActiveView::Connection(id) => open
                .iter()
                .find(|(open_id, _)| *open_id == id)
                .map(|(_, locator)| locator.clone()),
            ActiveView::Connections => None,
        };
        Session { tabs, active }
    }

    /// Persists the current session (best-effort, off the UI path). Called by
    /// the persistence effect only when the snapshot actually changed.
    pub fn persist_session(&self) {
        let Some(path) = default_session_path() else {
            return;
        };
        let session = self.current_session();
        spawn_forever(async move {
            let _ = save_session(&path, &session);
        });
    }

    /// Reopens the previous session's tabs (FRE-30). Runs once at startup.
    ///
    /// Only connections still in the saved list are reopened (ad-hoc ones are
    /// not resurrected). SQLite reconnects silently; Postgres reconnects only
    /// when its password is already available (session memory or keyring), so
    /// startup never raises a wall of password prompts — a saved Postgres
    /// connection whose password isn't stored is simply left for the user to
    /// click. A locator whose file/server is gone just fails to reopen through
    /// the normal connect-error path; it never blocks the rest of the restore.
    pub async fn restore_session(mut self) {
        let Some(path) = default_session_path() else {
            return;
        };
        let session = load_session(&path);
        if session.tabs.is_empty() {
            return;
        }
        // Snapshot the saved list in canonical open-locator form up front.
        let saved: Vec<SavedConnection> = self.saved.read().entries().to_vec();
        let candidates: Vec<RestoreCandidate> = saved
            .iter()
            .map(|s| RestoreCandidate {
                locator: saved_open_locator(s),
                backend: s.backend(),
            })
            .collect();
        // Which server-backend locators (Postgres, SQL Server) can connect
        // silently, so startup never blocks on a prompt or pops a browser?
        // Password auth needs a stored/session password; Entra managed
        // identity always can; Entra interactive only with a cached refresh
        // token. The keyring reads are off-thread and the session-memory
        // borrow is dropped before the await.
        let mut ready_locators: HashSet<String> = HashSet::new();
        for candidate in &candidates {
            match candidate.backend {
                // SQLite needs no credentials; plan_session_restore always
                // keeps it.
                BackendKind::Sqlite => continue,
                // Both fall through to the auth-availability check below.
                BackendKind::Postgres | BackendKind::SqlServer => {}
            }
            let auth = saved.iter().find_map(|s| match s {
                SavedConnection::Postgres { auth, .. }
                | SavedConnection::SqlServer { auth, .. }
                    if saved_open_locator(s) == candidate.locator =>
                {
                    Some(auth.clone())
                }
                _ => None,
            });
            let ready = match auth {
                Some(PgAuth::Entra(entra)) => {
                    let has_refresh =
                        crate::secrets::get_password_async(entra_secret_key(&candidate.locator))
                            .await
                            .ok()
                            .flatten()
                            .is_some();
                    entra.can_acquire_silently(has_refresh)
                }
                _ => {
                    let in_session = self
                        .session_passwords
                        .read()
                        .contains_key(&candidate.locator);
                    in_session
                        || crate::secrets::get_password_async(candidate.locator.clone())
                            .await
                            .ok()
                            .flatten()
                            .is_some()
                }
            };
            if ready {
                ready_locators.insert(candidate.locator.clone());
            }
        }
        let plan = plan_session_restore(&session.tabs, &candidates, |loc| {
            ready_locators.contains(loc)
        });
        for tab in &plan {
            let saved_conn = saved
                .iter()
                .find(|s| saved_open_locator(s) == tab.locator)
                .cloned();
            let Some(saved_conn) = saved_conn else {
                continue;
            };
            let backend = ServerBackend::of(saved_conn.backend());
            match saved_conn {
                SavedConnection::Sqlite { path, .. } => self.connect(path).await,
                SavedConnection::Postgres {
                    url,
                    name,
                    tunnel,
                    auth,
                    ..
                }
                | SavedConnection::SqlServer {
                    url,
                    name,
                    tunnel,
                    auth,
                    ..
                } => self.connect_server(backend, url, name, tunnel, auth).await,
            }
            // Apply the remembered table, pane, and row detail panel (FRE-109)
            // to the freshly opened tab.
            let id = self
                .open_locators
                .read()
                .iter()
                .find(|(_, locator)| *locator == tab.locator)
                .map(|(id, _)| *id);
            if let Some(id) = id {
                let selected = tab.selected_table.as_ref().map(|name| TableRef {
                    schema: tab.selected_schema.clone(),
                    name: name.clone(),
                });
                let pane = Pane::from_session(tab.pane);
                let mut tab_ui = self.tab_ui.write();
                let ui = tab_ui.entry(id).or_default();
                ui.selected_table = selected;
                ui.pane = pane;
                ui.row_detail = tab.row_detail;
            }
        }
        // Restore the active view: the remembered active tab if it reopened,
        // else the connections screen. (Each connect above set `active` to its
        // own tab, so this override runs last.)
        let active_id = session.active.as_ref().and_then(|locator| {
            self.open_locators
                .read()
                .iter()
                .find(|(_, l)| l == locator)
                .map(|(id, _)| *id)
        });
        match active_id {
            Some(id) => self.active.set(ActiveView::Connection(id)),
            None => self.active.set(ActiveView::Connections),
        }
    }
}

impl Default for AppState {
    fn default() -> Self {
        Self::new()
    }
}

/// Reads the OS dark-mode preference through the webview's `matchMedia`.
/// Any failure (eval error, closed channel, non-bool result) is treated as
/// "light" — a safe, legible default.
async fn system_prefers_dark() -> bool {
    let mut eval =
        document::eval("dioxus.send(window.matchMedia('(prefers-color-scheme: dark)').matches);");
    eval.recv::<bool>().await.unwrap_or(false)
}

/// Canonicalizes for dedupe purposes; falls back to the given path when the
/// file is missing (the connect attempt will surface that error).
pub(crate) fn canonical(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

/// The open-locator form of a saved connection: [`connect_key`] applied to a
/// saved entry. Kept separate rather than delegating, so the SQLite path is
/// canonicalized straight from the `PathBuf` instead of round-tripping
/// through `locator()`'s lossy `display()` first.
pub(crate) fn saved_open_locator(saved: &SavedConnection) -> String {
    match saved {
        SavedConnection::Sqlite { path, .. } => canonical(path).display().to_string(),
        SavedConnection::Postgres { url, .. } | SavedConnection::SqlServer { url, .. } => {
            url.clone()
        }
    }
}

/// Sibling temp path for an atomic export write (`foo.csv` → `foo.csv.part`).
/// Streaming into this and renaming on success keeps a mid-stream failure
/// from clobbering an existing file at the destination.
fn export_temp_path(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".part");
    path.with_file_name(name)
}

/// What a Save click should do on a write-protected connection (FRE-111).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SaveAction {
    /// Send the changes now.
    Apply,
    /// Hold them and show the connection-naming confirmation first.
    Park,
}

/// Resolves a Save click: `protection` is the connection's marking, `parked`
/// the change list a previous click already confirmed (if any), and `current`
/// what the stage holds now.
///
/// The comparison between `parked` and `current` is the substance here.
/// Confirming authorizes a *specific* set of changes, so editing, discarding
/// or staging more after the prompt appears has to send the user back through
/// it — otherwise the prompt would launder whatever the stage happened to
/// contain by the time they clicked, which is exactly the accident FRE-111
/// exists to prevent.
///
/// `ReadOnly` returns `Apply` rather than a third variant: there is nothing to
/// confirm, and the write is refused underneath by the capability resolution
/// (see [`TableAccess::resolve_protected`]) with a message naming the marking.
/// Parking it instead would offer a prompt whose only outcome is a failure.
fn save_action(
    protection: WriteProtection,
    parked: Option<&[StagedChange]>,
    current: &[StagedChange],
) -> SaveAction {
    if !protection.confirms() {
        return SaveAction::Apply;
    }
    match parked {
        Some(confirmed) if confirmed == current => SaveAction::Apply,
        _ => SaveAction::Park,
    }
}

/// Tab label for a database file: the file name, or the whole path when
/// there is no file name component.
pub fn tab_title(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn staged_has_dirty_flags_any_pending_edit_anywhere() {
        use crate::db::RowLocator;
        let mut registry = ConnectionRegistry::default();
        let pool =
            DbPool::Sqlite(sqlx::sqlite::SqlitePool::connect_lazy("sqlite::memory:").unwrap());
        let id = registry.insert("t.db", pool, WriteProtection::Open);

        // Empty map: nothing to lose.
        let mut staged: HashMap<ConnectionId, HashMap<String, TableStage>> = HashMap::new();
        assert!(!staged_has_dirty(&staged));

        // A present-but-empty stage still isn't dirty (empties are usually
        // pruned, but the guard must not trip on a lingering one).
        let mut tables: HashMap<String, TableStage> = HashMap::new();
        tables.insert("public.t".to_string(), TableStage::default());
        staged.insert(id, tables);
        assert!(!staged_has_dirty(&staged));

        // One pending delete anywhere makes the whole app dirty.
        staged
            .get_mut(&id)
            .unwrap()
            .get_mut("public.t")
            .unwrap()
            .mark_delete(RowLocator {
                identity_values: vec![crate::db::Value::Integer(1)],
            });
        assert!(staged_has_dirty(&staged));
    }

    #[tokio::test]
    async fn nav_guard_confirm_needs_a_matching_intent_and_the_time_floor() {
        // A registry-issued id (ConnectionId is opaque outside db).
        let mut registry = ConnectionRegistry::default();
        let pool =
            DbPool::Sqlite(sqlx::sqlite::SqlitePool::connect_lazy("sqlite::memory:").unwrap());
        let id = registry.insert("t.db", pool, WriteProtection::Open);

        let action = NavAction::CloseConnection;
        let fresh = PendingNav::new(id, action.clone());
        assert!(fresh.matches(id, &action));
        assert!(
            !fresh.confirmable(),
            "an immediate identical repeat (double-click) must not confirm"
        );
        let aged = PendingNav {
            parked_at: Instant::now() - NAV_CONFIRM_MIN_DELAY,
            ..fresh
        };
        assert!(aged.confirmable(), "a deliberate later repeat confirms");
        assert!(
            !aged.matches(id, &NavAction::SetPane(Pane::Sql)),
            "a different action never confirms a parked intent"
        );
    }

    #[test]
    fn a_confirm_marked_connection_parks_the_first_save_and_applies_the_confirmed_one() {
        let changes = vec![delete(1)];
        // First click: nothing parked yet, so the prompt goes up.
        assert_eq!(
            save_action(WriteProtection::Confirm, None, &changes),
            SaveAction::Park
        );
        // Second click, stage unchanged: the confirmation stands.
        assert_eq!(
            save_action(WriteProtection::Confirm, Some(&changes), &changes),
            SaveAction::Apply
        );
    }

    #[test]
    fn a_confirmation_goes_stale_when_the_stage_moves_on() {
        // The hole this closes: a parked confirmation named ONE change. If the
        // stage grows, shrinks or changes before the user clicks through,
        // applying without re-prompting would write something they never read.
        let confirmed = vec![delete(1)];
        for moved_on in [
            vec![delete(1), delete(2)], // staged more
            vec![delete(2)],            // swapped for a different row
            vec![],                     // discarded
        ] {
            assert_eq!(
                save_action(WriteProtection::Confirm, Some(&confirmed), &moved_on),
                SaveAction::Park,
                "{moved_on:?} must send the user back through the prompt"
            );
        }
    }

    #[test]
    fn an_unmarked_connection_never_parks_a_save() {
        let changes = vec![delete(1)];
        assert_eq!(
            save_action(WriteProtection::Open, None, &changes),
            SaveAction::Apply,
            "Open must behave exactly as it did before FRE-111"
        );
    }

    #[test]
    fn a_read_only_marking_does_not_prompt_because_there_is_nothing_to_confirm() {
        // The write is refused underneath by the capability resolution; a
        // prompt here would have only one possible outcome — failure.
        let changes = vec![delete(1)];
        assert_eq!(
            save_action(WriteProtection::ReadOnly, None, &changes),
            SaveAction::Apply
        );
    }

    #[test]
    fn a_backend_descriptor_carries_its_engine_s_data() {
        // The whole point of the descriptor (FRE-139): every per-engine
        // difference the connect flows used to branch on is one lookup here,
        // so a wrong pairing is visible in one place.
        let pg = ServerBackend::POSTGRES;
        let ms = ServerBackend::SQL_SERVER;
        assert_eq!(pg.kind, BackendKind::Postgres);
        assert_eq!(ms.kind, BackendKind::SqlServer);
        // The OAuth resources are not interchangeable: a token minted for one
        // service is rejected by the other.
        assert_eq!(pg.entra_resource, azure::OSSRDBMS_RESOURCE);
        assert_eq!(ms.entra_resource, azure::SQLDB_RESOURCE);
        assert_ne!(pg.entra_resource, ms.entra_resource);
        // URL helpers are the engine's own — note the default ports, which is
        // where a mixed-up pairing would show first.
        assert_eq!(
            (pg.url_target)("postgres://u@db.internal/app").unwrap(),
            ("db.internal".to_string(), 5432)
        );
        assert_eq!(
            (ms.url_target)("mssql://u@db.internal/app").unwrap(),
            ("db.internal".to_string(), 1433)
        );
        assert_eq!(
            (pg.with_password)("postgres://u@h:5432/db", "p@ss").unwrap(),
            "postgres://u:p%40ss@h:5432/db"
        );
        assert_eq!(
            (ms.with_password)("mssql://sa@h:1433/db", "p@ss").unwrap(),
            "mssql://sa:p%40ss@h:1433/db"
        );
        assert_eq!(
            (ms.via_local_port)("mssql://sa@db.internal:1433/db", 40123).unwrap(),
            "mssql://sa@127.0.0.1:40123/db"
        );
    }

    #[test]
    fn every_backend_kind_resolves_to_a_server_descriptor() {
        assert_eq!(
            ServerBackend::of(BackendKind::SqlServer).kind,
            BackendKind::SqlServer
        );
        assert_eq!(
            ServerBackend::of(BackendKind::Postgres).kind,
            BackendKind::Postgres
        );
        // SQLite never reaches a server connect flow (no URL, no auth, no
        // prompts); it resolves to Postgres exactly as the `if SqlServer {…}
        // else {…}` dispatch this replaced did.
        assert_eq!(
            ServerBackend::of(BackendKind::Sqlite).kind,
            BackendKind::Postgres
        );
    }

    #[test]
    fn only_a_tunneled_sql_server_connect_overrides_the_tls_host() {
        let pg = ServerBackend::POSTGRES;
        let ms = ServerBackend::SQL_SERVER;
        // Through a tunnel the connect URL points at 127.0.0.1, so TLS has to
        // be told the server's real hostname (FRE-58).
        assert_eq!(
            ms.tls_host("mssql://sa@db.example.com:1433/app", true),
            Some("db.example.com".to_string())
        );
        assert_eq!(
            ms.tls_host("mssql://sa@db.example.com:1433/app", false),
            None
        );
        // Postgres has no such parameter — the MSSQL-only override must never
        // leak into its path, tunneled or not.
        assert_eq!(
            pg.tls_host("postgres://u@db.example.com:5432/app", true),
            None
        );
        assert_eq!(
            pg.tls_host("postgres://u@db.example.com:5432/app", false),
            None
        );
    }

    #[test]
    fn a_saved_entry_takes_the_backend_s_variant() {
        let tunnel = None;
        let pg = ServerBackend::POSTGRES.saved(
            "prod",
            "postgres://u@h:5432/db",
            tunnel.clone(),
            PgAuth::Password,
        );
        assert!(matches!(pg, SavedConnection::Postgres { .. }));
        assert_eq!(pg.backend(), BackendKind::Postgres);
        assert_eq!(pg.name(), "prod");
        assert_eq!(pg.locator(), "postgres://u@h:5432/db");
        // A fresh entry is unmarked; markings are set from the connections
        // list, never inferred by a connect (FRE-111).
        assert_eq!(pg.protection(), WriteProtection::Open);
        assert_eq!(pg.color(), None);

        let ms = ServerBackend::SQL_SERVER.saved(
            "reporting",
            "mssql://sa@h:1433/db",
            tunnel,
            PgAuth::Entra(EntraAuth::interactive_default()),
        );
        assert!(matches!(ms, SavedConnection::SqlServer { .. }));
        assert_eq!(ms.backend(), BackendKind::SqlServer);
        match ms {
            SavedConnection::SqlServer { auth, .. } => {
                assert!(matches!(auth, PgAuth::Entra(_)), "auth mode is preserved")
            }
            _ => unreachable!(),
        }
    }

    fn delete(id: i64) -> StagedChange {
        StagedChange::Delete {
            locator: crate::db::RowLocator {
                identity_values: vec![Value::Integer(id)],
            },
        }
    }

    #[test]
    fn tab_title_uses_the_file_name() {
        assert_eq!(tab_title(Path::new("/data/apps/music.db")), "music.db");
        assert_eq!(tab_title(Path::new("plain.sqlite")), "plain.sqlite");
    }

    #[test]
    fn tab_title_falls_back_to_the_full_path() {
        assert_eq!(tab_title(Path::new("/")), "/");
    }

    #[test]
    fn canonical_falls_back_for_missing_files() {
        let missing = Path::new("/definitely/not/here.db");
        assert_eq!(canonical(missing), missing.to_path_buf());
    }

    #[test]
    fn connect_key_canonicalizes_only_sqlite_paths() {
        // A server URL is already the open-locator form: canonicalizing it as
        // a path would mangle it, and the connections list would then never
        // match a row against its own in-flight connect.
        let url = "postgres://user@host:5432/db";
        assert_eq!(connect_key(url, BackendKind::Postgres), url);
        assert_eq!(connect_key(url, BackendKind::SqlServer), url);

        // SQLite keys on the canonical path, matching what `connect` reserves
        // and what `open_locators` records. Uses a real file, since
        // `canonical` falls back verbatim for missing ones.
        let dir = std::env::temp_dir().join("hubro-connect-key-test");
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("demo.db");
        std::fs::write(&db, b"").unwrap();
        let indirect = dir.join(".").join("demo.db");
        assert_eq!(
            connect_key(&indirect.display().to_string(), BackendKind::Sqlite),
            canonical(&db).display().to_string(),
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn connect_step_labels_are_distinct_and_lowercase() {
        let steps = [
            ConnectStep::Tunnel,
            ConnectStep::Credentials,
            ConnectStep::SigningIn,
            ConnectStep::Opening,
        ];
        let labels: HashSet<&str> = steps.iter().map(|s| s.label()).collect();
        assert_eq!(labels.len(), steps.len(), "two steps read the same");
        for step in steps {
            let label = step.label();
            // These render as a status line under the connection name, not as
            // a sentence, so they stay lowercase and unfinished.
            assert!(label.ends_with('…'), "{label:?} lacks a trailing ellipsis");
            assert!(
                !label.starts_with(|c: char| c.is_uppercase()),
                "{label:?} is capitalized"
            );
        }
    }
}
