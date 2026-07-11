use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use dioxus::core::{spawn_forever, Task};
use dioxus::prelude::*;

use crate::config::{
    default_config_path, default_settings_path, load_settings, save_settings, SavedConnection,
    SavedList, Settings, Theme,
};
use crate::db::{
    apply_staged, detect_row_identity, needs_confirmation, run_script, split_statements,
    url_target, url_via_local_port, url_with_password, write_result, ConnectionId,
    ConnectionRegistry, DbError, DbPool, ExportFormat, QueryResult, RowLocator, StatementResult,
    TableMeta, Value,
};
use crate::history::HistoryStore;
use crate::tunnel::{Tunnel, TunnelAuth, TunnelConfig, TunnelError};
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

/// Which pane a connection tab shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Pane {
    #[default]
    Browser,
    Sql,
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
    /// Tables expanded in the sidebar tree, by [`TableRef::key`].
    pub expanded: HashSet<String>,
    /// Data browser vs SQL editor.
    pub pane: Pane,
    /// SQL editor buffer, synced from the webview so it survives pane and
    /// tab switches.
    pub sql_text: String,
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
    /// [`SqlRun::statements`].
    Failed {
        error: String,
        statement_index: usize,
        preview: String,
        elapsed_ms: u64,
    },
    /// The user aborted the run. Outcomes of the statements that finished
    /// before the abort stay visible; the in-flight statement may still
    /// complete server-side (see [`AppState::cancel_sql`]).
    Cancelled,
}

/// State of the most recent SQL script run per connection.
#[derive(Debug, Clone, PartialEq)]
pub struct SqlRun {
    /// Outcomes of the statements that finished, in script order.
    pub statements: Vec<StatementResult>,
    pub status: RunStatus,
}

/// Progress of the most recent export (grid or SQL result) per connection.
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

impl TabUi {
    /// Flips one table's expansion state.
    pub fn toggle_expanded(&mut self, table: &str) {
        if !self.expanded.remove(table) {
            self.expanded.insert(table.to_string());
        }
    }
}

/// What a pending [`PasswordPrompt`] is asking for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptKind {
    /// The database password.
    DbPassword,
    /// The passphrase decrypting the SSH tunnel's key file.
    SshPassphrase,
}

/// A pending secret request for a saved Postgres connection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PasswordPrompt {
    pub url: String,
    pub name: String,
    pub kind: PromptKind,
    /// Tunnel settings of the connect attempt, carried through the prompt so
    /// the retry resumes the same flow.
    pub tunnel: Option<TunnelConfig>,
}

/// Keyring/session key for a connection's SSH key passphrase. The `#ssh`
/// suffix keeps it disjoint from the database password stored under the
/// bare URL (`#` cannot appear in a valid connection URL's serialized form
/// unescaped, so this never collides).
pub(crate) fn ssh_secret_key(url: &str) -> String {
    format!("{url}#ssh")
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
    /// Locators with a connect in flight, reserved before the pool open
    /// await.
    pub connecting: Signal<Vec<String>>,
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
    /// (The grid's resource reads the whole map, so a bump for one table
    /// re-runs any mounted grid's fetch — harmless, since only the selected
    /// table's grid is mounted.)
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
    /// Not guarded yet: closing the OS window discards every stage without
    /// warning — follow-up in FRE-30 (session persistence) / FRE-18.
    pub nav_guard: Signal<Option<PendingNav>>,
    /// Latest free-form SQL result per connection.
    pub sql_runs: Signal<HashMap<ConnectionId, SqlRun>>,
    /// Scripts containing writes, held here until the user confirms (or
    /// dismisses) the write-confirmation banner.
    pub pending_sql: Signal<HashMap<ConnectionId, PendingSql>>,
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
    /// Latest export progress per connection (grid or SQL result). Root-
    /// scoped: written from the `spawn_forever` export task.
    pub export_status: Signal<HashMap<ConnectionId, ExportStatus>>,
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
        // Theme preference: best-effort load, defaults on any problem
        // (settings are non-critical and never block the app).
        let theme = default_settings_path()
            .map(|path| load_settings(&path).theme)
            .unwrap_or_default();
        let state = Self {
            registry: Signal::new(ConnectionRegistry::default()),
            active: Signal::new(ActiveView::Connections),
            saved: Signal::new(saved),
            open_locators: Signal::new(Vec::new()),
            connecting: Signal::new(Vec::new()),
            connect_error: Signal::new(load_error),
            session_passwords: Signal::new(HashMap::new()),
            password_prompt: Signal::new(None),
            tunnels: Signal::new(HashMap::new()),
            schemas: Signal::new(HashMap::new()),
            tab_ui: Signal::new(HashMap::new()),
            // Root-scoped: written from the spawn_forever save task.
            staged: Signal::new_in_scope(HashMap::new(), ScopeId::ROOT),
            grid_refresh: Signal::new_in_scope(HashMap::new(), ScopeId::ROOT),
            nav_guard: Signal::new(None),
            sql_runs: Signal::new(HashMap::new()),
            pending_sql: Signal::new(HashMap::new()),
            sql_tasks: Signal::new(HashMap::new()),
            sql_generations: Signal::new(HashMap::new()),
            // Root-scoped: these are written from `spawn_forever` tasks
            // (which run in the root scope), so the component scope that
            // built the state must not own them.
            history: Signal::new_in_scope(None, ScopeId::ROOT),
            history_error: Signal::new_in_scope(None, ScopeId::ROOT),
            history_nonce: Signal::new_in_scope(0, ScopeId::ROOT),
            history_recording: Signal::new_in_scope(true, ScopeId::ROOT),
            theme: Signal::new(theme),
            // Start from the persisted theme assuming a light system default;
            // the startup detection task (below) corrects `System`. Root-
            // scoped: written from that spawn_forever task.
            dark: Signal::new_in_scope(theme.resolve_dark(false), ScopeId::ROOT),
            // Root-scoped: written from the spawn_forever export task.
            export_status: Signal::new_in_scope(HashMap::new(), ScopeId::ROOT),
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
        state
    }

    /// Adds a database file to the saved list (deduped by path) and
    /// persists the list.
    pub fn add_saved(mut self, path: PathBuf) {
        let path = canonical(&path);
        let added = self.saved.write().add(SavedConnection::Sqlite {
            name: tab_title(&path),
            path,
        });
        if added {
            self.persist_saved();
        }
    }

    /// Adds a Postgres connection to the saved list (URL stored without a
    /// password; tunnel settings, sans passphrase, stored alongside) and
    /// persists.
    pub fn add_saved_postgres(mut self, name: String, url: String, tunnel: Option<TunnelConfig>) {
        let added = self
            .saved
            .write()
            .add(SavedConnection::Postgres { name, url, tunnel });
        if added {
            self.persist_saved();
        }
    }

    /// Removes a saved connection (open tabs are unaffected) and persists.
    /// Postgres entries also drop their keyring credentials (database
    /// password and SSH key passphrase).
    pub fn remove_saved(mut self, locator: &str) {
        let removed = self.saved.write().remove(locator);
        if let Some(entry) = removed {
            if let SavedConnection::Postgres { url, .. } = entry {
                // Best-effort, off-thread: a missing keyring just means
                // nothing was stored.
                spawn_forever(async move {
                    let _ = crate::secrets::delete_password_async(url.clone()).await;
                    let _ = crate::secrets::delete_password_async(ssh_secret_key(&url)).await;
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

    /// Opens a saved Postgres connection. With a tunnel configured, the
    /// tunnel opens first (its failures surface as "SSH tunnel: …", distinct
    /// from database errors) and Postgres connects through the forwarded
    /// port. Uses the session password when one is known; otherwise tries
    /// without and falls back to a password prompt on authentication failure
    /// (so trust-auth servers connect silently).
    pub async fn connect_postgres(
        mut self,
        url: String,
        name: String,
        tunnel: Option<TunnelConfig>,
    ) {
        self.connect_error.set(None);
        if self.focus_or_reserve(&url) {
            return;
        }
        let Some((connect_url, live_tunnel)) = self.open_tunnel(&url, &name, &tunnel).await else {
            return; // failure already surfaced (error or passphrase prompt)
        };
        // Session memory first, then the OS keyring. The keyring call runs
        // off-thread (a locked wallet can block on a user dialog) and only
        // after the session read guard is dropped; errors mean "no keyring"
        // and fall through to the prompt flow.
        let mut session_password = self.session_passwords.read().get(&url).cloned();
        if session_password.is_none() {
            session_password = crate::secrets::get_password_async(url.clone())
                .await
                .ok()
                .flatten();
        }
        let had_password = session_password.is_some();
        let result = match &session_password {
            Some(password) => match url_with_password(&connect_url, password) {
                Ok(full) => DbPool::open_postgres(&full).await,
                Err(err) => Err(err),
            },
            None => DbPool::open_postgres(&connect_url).await,
        };
        match result {
            Err(DbError::Connect(msg)) if msg.contains("authentication failed") => {
                self.connecting.write().retain(|l| l != &url);
                if had_password {
                    // Stored password is stale; drop it everywhere and re-ask.
                    self.session_passwords.write().remove(&url);
                    let _ = crate::secrets::delete_password_async(url.clone()).await;
                    self.connect_error
                        .set(Some(format!("connection failed: {msg}")));
                }
                // live_tunnel drops here; the retry re-opens it.
                self.password_prompt.set(Some(PasswordPrompt {
                    url,
                    name,
                    kind: PromptKind::DbPassword,
                    tunnel,
                }));
            }
            result => {
                self.finish_connect(url.clone(), name.clone(), result, live_tunnel);
                self.save_postgres_if_open(&url, &name, tunnel);
            }
        }
    }

    /// Completes the password prompt: connects with the entered password
    /// (through the tunnel when one is configured). On success the password
    /// always lives in session memory; with `remember` it is also stored in
    /// the OS keyring (silently staying session-only when no keyring is
    /// available).
    pub async fn connect_postgres_with_password(
        mut self,
        url: String,
        name: String,
        password: String,
        remember: bool,
        tunnel: Option<TunnelConfig>,
    ) {
        self.connect_error.set(None);
        // The prompt replaces the reservation made by connect_postgres, so
        // re-reserve here.
        if self.focus_or_reserve(&url) {
            return;
        }
        let Some((connect_url, live_tunnel)) = self.open_tunnel(&url, &name, &tunnel).await else {
            return;
        };
        let result = match url_with_password(&connect_url, &password) {
            Ok(full) => DbPool::open_postgres(&full).await,
            Err(err) => Err(err),
        };
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
        self.save_postgres_if_open(&url, &name, tunnel);
    }

    /// Completes the SSH-passphrase prompt: remembers the passphrase for the
    /// session and re-runs the connect flow (which now finds it). Keyring
    /// persistence happens only after the connect succeeded, so a mistyped
    /// passphrase is never stored.
    pub async fn connect_postgres_with_ssh_passphrase(
        mut self,
        url: String,
        name: String,
        tunnel: TunnelConfig,
        passphrase: String,
        remember: bool,
    ) {
        self.stash_ssh_passphrase(&url, passphrase);
        self.password_prompt.set(None);
        self.connect_postgres(url.clone(), name, Some(tunnel)).await;
        let connected = self.open_locators.read().iter().any(|(_, l)| *l == url);
        if remember && connected {
            self.persist_ssh_passphrase(&url).await;
        }
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
    /// live tunnel. `None` means the attempt already ended: the reservation
    /// was released and either an error was surfaced or the passphrase
    /// prompt was raised.
    async fn open_tunnel(
        mut self,
        url: &str,
        name: &str,
        tunnel: &Option<TunnelConfig>,
    ) -> Option<(String, Option<Tunnel>)> {
        let Some(config) = tunnel else {
            return Some((url.to_string(), None));
        };
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
        let target = match url_target(url) {
            Ok(target) => target,
            Err(err) => {
                self.fail_connect(url, err.to_string());
                return None;
            }
        };
        match Tunnel::open(config.clone(), passphrase, target.0, target.1).await {
            Ok(live) => match url_via_local_port(url, live.local_port()) {
                Ok(rewritten) => Some((rewritten, Some(live))),
                Err(err) => {
                    self.fail_connect(url, err.to_string());
                    None
                }
            },
            Err(err @ TunnelError::NeedsPassphrase(_)) => {
                self.connecting.write().retain(|l| l != url);
                if had_passphrase {
                    // Stored passphrase is stale; drop it everywhere and
                    // re-ask.
                    self.session_passwords.write().remove(&secret_key);
                    let _ = crate::secrets::delete_password_async(secret_key).await;
                    self.connect_error.set(Some(err.to_string()));
                }
                self.password_prompt.set(Some(PasswordPrompt {
                    url: url.to_string(),
                    name: name.to_string(),
                    kind: PromptKind::SshPassphrase,
                    tunnel: Some(config.clone()),
                }));
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
        self.connecting.write().retain(|l| l != locator);
        self.connect_error.set(Some(message));
    }

    /// A successful Postgres connect always joins the saved list (add is a
    /// no-op when URL and tunnel are already saved, and updates the tunnel
    /// of an existing entry otherwise). This keeps the "connect first, save
    /// on success" contract even when the connect went through a prompt
    /// instead of the form's direct path.
    fn save_postgres_if_open(self, url: &str, name: &str, tunnel: Option<TunnelConfig>) {
        let is_open = self.open_locators.read().iter().any(|(_, l)| l == url);
        if is_open {
            self.add_saved_postgres(name.to_string(), url.to_string(), tunnel);
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
            self.active.set(ActiveView::Connection(id));
            return true;
        }
        let mut connecting = self.connecting.write();
        if connecting.iter().any(|l| l == locator) {
            return true;
        }
        connecting.push(locator.to_string());
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
        self.connecting.write().retain(|l| l != &locator);
        match result {
            Ok(pool) => {
                let id = self.registry.write().insert(name, pool);
                if let Some(tunnel) = tunnel {
                    self.tunnels.write().insert(id, tunnel);
                }
                self.open_locators.write().push((id, locator));
                self.active.set(ActiveView::Connection(id));
                self.load_schema(id);
            }
            Err(err) => {
                drop(tunnel); // a tunnel without its database is useless
                self.connect_error.set(Some(err.to_string()));
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

    /// Switches a tab between the data browser and the SQL editor. Guarded:
    /// leaving the browser while the selected table has staged edits takes
    /// two attempts (see [`Self::nav_guard`]); the second discards them.
    /// While that table's save is in flight the switch no-ops.
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
    pub fn run_sql(mut self, id: ConnectionId, sql: String) {
        self.pending_sql.write().remove(&id);
        let statements = split_statements(&sql);
        if statements.is_empty() {
            return;
        }
        if statements.iter().any(|s| needs_confirmation(s)) {
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

    /// Executes a split script in the background: reads fetch rows, writes
    /// report affected counts, execution stops at the first error. Each
    /// statement's outcome lands in [`Self::sql_runs`] as it finishes.
    fn execute_script(mut self, id: ConnectionId, script: String, statements: Vec<String>) {
        let Some(pool) = self.registry.read().get(id).map(|c| c.pool.clone()) else {
            return;
        };
        // A re-run replaces any still-running task for this connection.
        let previous = self.sql_tasks.write().remove(&id);
        if let Some(previous) = previous {
            previous.cancel();
        }
        let generation = {
            let mut generations = self.sql_generations.write();
            let entry = generations.entry(id).or_insert(0);
            *entry += 1;
            *entry
        };
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
            let result = run_script(&pool, &statements, |statement| {
                if self.sql_generation(id) == generation {
                    if let Some(run) = self.sql_runs.write().get_mut(&id) {
                        run.statements.push(statement);
                    }
                }
            })
            .await;
            let elapsed_ms = started.elapsed().as_millis() as u64;
            // History is recorded even when a newer run made this one stale —
            // the script did execute. Cancelled runs never reach this point
            // (the future is dropped), so they are not recorded.
            let error_text = result.as_ref().err().map(|e| e.error.to_string());
            self.record_history(id, script, result.is_ok(), error_text)
                .await;
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
    /// nonce bump afterwards tells open history panels to re-query.
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
        let recorded = store
            .record(&locator, &script, success, error.as_deref())
            .await;
        if recorded.unwrap_or(false) {
            let mut nonce = self.history_nonce.write();
            *nonce += 1;
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
        let Some(path) = default_settings_path() else {
            return;
        };
        let settings = Settings { theme };
        spawn_forever(async move {
            let _ = save_settings(&path, &settings);
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
    pub fn select_table(mut self, id: ConnectionId, table: &TableRef) {
        let current = self
            .tab_ui
            .read()
            .get(&id)
            .and_then(|ui| ui.selected_table.clone());
        if current.as_ref() == Some(table) {
            return;
        }
        if let Some(current_table) = &current {
            if self.stage_dirty(id, current_table) {
                if self.stage_saving(id, current_table) {
                    return;
                }
                if !self.nav_guard_allows(id, NavAction::SelectTable(table.clone())) {
                    return;
                }
                self.discard_staged(id, current_table);
            }
        }
        self.nav_guard.set(None);
        self.tab_ui.write().entry(id).or_default().selected_table = Some(table.clone());
    }

    /// Flips a table's expansion state in one tab's sidebar tree.
    pub fn toggle_expanded(mut self, id: ConnectionId, table: &str) {
        self.tab_ui
            .write()
            .entry(id)
            .or_default()
            .toggle_expanded(table);
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
    pub fn save_staged(mut self, id: ConnectionId, table: &TableRef) {
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
        let Some(identity) = detect_row_identity(&meta, pool.dialect()) else {
            self.fail_save(
                id,
                &table_key,
                "this table has no usable row identity — it is read-only".into(),
            );
            return;
        };
        // spawn_forever: the save must survive the grid unmounting (e.g. a
        // guarded navigation completing while the apply runs).
        spawn_forever(async move {
            let result = apply_staged(&pool, &meta, &identity, &changes).await;
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
        let pool = self.registry.read().get(id).map(|c| c.pool.clone());
        let Some(pool) = pool else {
            self.export_status
                .write()
                .insert(id, ExportStatus::Failed("connection closed".into()));
            return;
        };
        self.export_status.write().insert(id, ExportStatus::Running);
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
            self.finish_export(id, outcome);
        });
    }

    /// Writes an already-materialized [`QueryResult`] (the SQL editor's held
    /// result) to `path` in `format`, in a background task. Shares the row
    /// formatters with [`Self::export_query`]; no database round-trip.
    pub fn export_result(
        mut self,
        id: ConnectionId,
        result: QueryResult,
        format: ExportFormat,
        path: PathBuf,
    ) {
        self.export_status.write().insert(id, ExportStatus::Running);
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
            self.finish_export(id, outcome);
        });
    }

    /// Records an export's terminal status.
    fn finish_export(mut self, id: ConnectionId, outcome: Result<u64, String>) {
        let status = match outcome {
            Ok(rows) => ExportStatus::Done { rows },
            Err(err) => ExportStatus::Failed(err),
        };
        self.export_status.write().insert(id, status);
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
    /// NOT guarded — follow-up in FRE-30 / FRE-18.)
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
        self.export_status.write().remove(&id);
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

/// Sibling temp path for an atomic export write (`foo.csv` → `foo.csv.part`).
/// Streaming into this and renaming on success keeps a mid-stream failure
/// from clobbering an existing file at the destination.
fn export_temp_path(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".part");
    path.with_file_name(name)
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
    async fn nav_guard_confirm_needs_a_matching_intent_and_the_time_floor() {
        // A registry-issued id (ConnectionId is opaque outside db).
        let mut registry = ConnectionRegistry::default();
        let pool =
            DbPool::Sqlite(sqlx::sqlite::SqlitePool::connect_lazy("sqlite::memory:").unwrap());
        let id = registry.insert("t.db", pool);

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
    fn toggle_expanded_flips_per_table() {
        let mut ui = TabUi::default();
        ui.toggle_expanded("artists");
        ui.toggle_expanded("albums");
        assert!(ui.expanded.contains("artists"));
        ui.toggle_expanded("artists");
        assert!(!ui.expanded.contains("artists"));
        assert!(ui.expanded.contains("albums"));
    }
}
