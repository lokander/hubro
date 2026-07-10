use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use dioxus::core::spawn_forever;
use dioxus::prelude::*;

use crate::config::{default_config_path, SavedConnection, SavedList};
use crate::db::{
    url_target, url_via_local_port, url_with_password, ConnectionId, ConnectionRegistry, DbError,
    DbPool, TableMeta,
};
use crate::tunnel::{Tunnel, TunnelAuth, TunnelConfig, TunnelError};

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

/// Per-tab UI state that must survive tab switches.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct TabUi {
    /// Table selected in the sidebar (shown in the data grid).
    pub selected_table: Option<TableRef>,
    /// Tables expanded in the sidebar tree, by [`TableRef::key`].
    pub expanded: HashSet<String>,
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
        Self {
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
        }
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

    /// Marks a table as selected in one tab's sidebar.
    pub fn select_table(mut self, id: ConnectionId, table: &TableRef) {
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

    /// Closes a tab: drops it from the registry, closes the pool in the
    /// background, shuts down its SSH tunnel (if any), and leaves the view
    /// somewhere sensible.
    pub fn close_connection(mut self, id: ConnectionId) {
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

/// Canonicalizes for dedupe purposes; falls back to the given path when the
/// file is missing (the connect attempt will surface that error).
pub(crate) fn canonical(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
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
