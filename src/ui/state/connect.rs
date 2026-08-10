//! Connection-lifecycle orchestration: the saved-connections list, the
//! per-engine connect flows (password, SSH tunnel, Microsoft Entra), and the
//! in-flight bookkeeping the connections screen renders.
//!
//! Split out of [`super`] because this is ~1,200 lines of procedural async
//! workflow rather than state definition: it reads and writes [`AppState`]'s
//! signals, but nothing here defines what the app *is*, only how a connection
//! comes to exist. The state layer proper — staging, SQL runs, session,
//! navigation — stays with the struct.

use super::*;

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
pub(super) fn ssh_secret_key(url: &str) -> String {
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
    /// A Microsoft Entra access token (never a refresh token). Owned, unlike
    /// its siblings: the token moves out of the acquisition and into the
    /// login, so a live access token is never copied into a second place.
    EntraToken(String),
}

/// How a credential logs in on one backend — everything [`ServerBackend::open`]
/// must decide before it touches the network, and nothing else.
///
/// Split out of the open because this decision is the crux of the backend
/// strategy and getting it wrong fails quietly in the worst way: a SQL Server
/// Entra token spliced into the URL would be sent as a *password* and rejected
/// as bad credentials, so the bug would read as an auth problem rather than a
/// routing one. Being pure, it is pinned by unit tests instead of by a live
/// server.
///
/// Deliberately no `Debug`: both fields carry a secret.
struct ServerLogin {
    /// The URL to dial, with the secret spliced in as its password when the
    /// engine takes it that way — which Postgres does for a password and an
    /// Entra token alike.
    url: String,
    /// The Entra access token to hand the driver *instead of* putting it in
    /// the URL: SQL Server's AAD login. `None` in every other case, Postgres
    /// tokens included — those live in `url`, and there is no driver parameter
    /// they could go to instead.
    aad_token: Option<String>,
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

    /// Where `credential` goes for this engine: into the URL as its password,
    /// or to the driver as an AAD login. Pure — see [`ServerLogin`] for why
    /// this is a step of its own.
    fn login(
        &self,
        connect_url: &str,
        credential: ServerCredential<'_>,
    ) -> Result<ServerLogin, DbError> {
        let spliced = |secret: &str| (self.with_password)(connect_url, secret);
        Ok(match (self.kind, credential) {
            // Nothing to place: the URL is dialed as it stands.
            (_, ServerCredential::None) => ServerLogin {
                url: connect_url.to_string(),
                aad_token: None,
            },
            // A password is a password on both engines.
            (_, ServerCredential::Password(password)) => ServerLogin {
                url: spliced(password)?,
                aad_token: None,
            },
            // SQL Server logs in with the token itself and ignores the URL's
            // user/password, so the token must stay out of the URL.
            (BackendKind::SqlServer, ServerCredential::EntraToken(token)) => ServerLogin {
                url: connect_url.to_string(),
                aad_token: Some(token),
            },
            // Postgres has no such login method: the token *is* the password.
            (_, ServerCredential::EntraToken(token)) => ServerLogin {
                url: spliced(&token)?,
                aad_token: None,
            },
        })
    }

    /// Opens the pool with `credential`, placed by [`Self::login`].
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
        let ServerLogin { url, aad_token } = self.login(connect_url, credential)?;
        match self.kind {
            BackendKind::SqlServer => {
                let auth = match aad_token {
                    Some(token) => MssqlAuth::AadToken(token),
                    None => MssqlAuth::Password,
                };
                DbPool::open_mssql_with(&url, &auth, tls_host).await
            }
            // No AAD login and no TLS host override exist here: `login` never
            // produces a token for Postgres (it goes into the URL), and sqlx
            // validates TLS against the URL's own host.
            _ => {
                debug_assert!(aad_token.is_none(), "a Postgres token belongs in the URL");
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
        auth: ServerAuth,
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
                group: None,
            },
            _ => SavedConnection::Postgres {
                name,
                url,
                tunnel,
                auth,
                protection: WriteProtection::Open,
                color: None,
                group: None,
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

impl AppState {
    /// Adds a database file to the saved list (deduped by path) and
    /// persists the list.
    pub fn add_saved(mut self, path: PathBuf) {
        let path = canonical(&path);
        let added = self.saved.write().add(SavedConnection::Sqlite {
            name: tab_title(&path),
            path,
            protection: WriteProtection::Open,
            color: None,
            group: None,
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

    /// Creates a connection group (FRE-120) and persists the list. The error
    /// is handed back for the connections screen to show next to the field
    /// the name was typed into.
    pub fn create_saved_group(mut self, name: &str) -> Result<String, GroupError> {
        // Scoped so the write borrow is released before `persist_saved`
        // takes a read one.
        let created = { self.saved.write().create_group(name) };
        if created.is_ok() {
            self.persist_saved();
        }
        created
    }

    /// Renames a group, carrying its members and its collapsed state with it.
    ///
    /// The fold is remembered by name in a different file (the session), so a
    /// rename that moved only the group would silently expand it — and leave
    /// a dead name behind to accumulate.
    pub fn rename_saved_group(mut self, old: &str, new: &str) -> Result<String, GroupError> {
        let renamed = { self.saved.write().rename_group(old, new) };
        let Ok(new_name) = renamed else {
            return renamed;
        };
        self.persist_saved();
        if new_name != old {
            let mut collapsed = self.collapsed_groups.write();
            for name in collapsed.iter_mut() {
                if name == old {
                    *name = new_name.clone();
                }
            }
        }
        Ok(new_name)
    }

    /// Removes a group; its connections become ungrouped rather than being
    /// removed with it.
    pub fn remove_saved_group(mut self, name: &str) {
        let removed = { self.saved.write().remove_group(name) };
        if !removed {
            return;
        }
        self.persist_saved();
        if self.collapsed_groups.peek().iter().any(|g| g == name) {
            self.collapsed_groups.write().retain(|g| g != name);
        }
    }

    /// Moves a group one step up or down the display order.
    pub fn move_saved_group(mut self, name: &str, up: bool) {
        let moved = { self.saved.write().move_group(name, up) };
        if moved {
            self.persist_saved();
        }
    }

    /// Files a saved connection under a group (FRE-120), or ungroups it with
    /// `None`. A connection is in at most one group, so this replaces rather
    /// than adds.
    ///
    /// Nothing about an *open* tab depends on the group — unlike the marking,
    /// which has to reach the live connection — so there is no counterpart
    /// here to [`Self::set_saved_marking`]'s re-marking step.
    pub fn assign_saved_group(mut self, locator: &str, group: Option<&str>) {
        let changed = { self.saved.write().assign_group(locator, group) };
        if changed {
            self.persist_saved();
        }
    }

    /// Folds or unfolds a group in the connections list. Persisted with the
    /// session by the shell's snapshot effect, which re-runs because
    /// `current_session` reads this signal.
    pub fn toggle_group_collapsed(mut self, name: &str) {
        let mut collapsed = self.collapsed_groups.write();
        match collapsed.iter().position(|g| g == name) {
            Some(index) => {
                collapsed.remove(index);
            }
            None => collapsed.push(name.to_string()),
        }
    }

    /// Records that the next successful connect to `new_locator` is an edit
    /// of `old_locator` rather than a new connection (FRE-75). Consumed by
    /// [`Self::save_server_if_open`], the one place every connect path saves
    /// through — including the Entra sign-in card, which completes long after
    /// the form has closed.
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
        auth: ServerAuth,
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
            ServerAuth::Entra(entra) => {
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
            ServerAuth::Password => {}
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
                    auth: ServerAuth::Password,
                }));
            }
            result => {
                self.finish_connect(url.clone(), name.clone(), result, live_tunnel);
                self.save_server_if_open(backend, &url, &name, tunnel, ServerAuth::Password);
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
        // The access token moves into the login (and no further): only the
        // refresh token below is ever written anywhere.
        let result = backend
            .open(
                connect_url,
                ServerCredential::EntraToken(token.secret),
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
            self.save_server_if_open(backend, &url, &name, tunnel, ServerAuth::Entra(entra));
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
                &ServerAuth::Entra(prompt.entra.clone()),
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
            self.save_server_if_open(backend, &url, &name, tunnel.clone(), ServerAuth::Password);
            return;
        }
        let Some((connect_url, live_tunnel)) = self
            .open_tunnel(&url, &name, &tunnel, &ServerAuth::Password, backend)
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
        self.save_server_if_open(backend, &url, &name, tunnel, ServerAuth::Password);
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
        auth: ServerAuth,
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
        auth: &ServerAuth,
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
        auth: ServerAuth,
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
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn a_credential_is_placed_where_its_engine_expects_it() {
        // The crux of the backend strategy, pinned for all three credentials
        // on both engines. A misplaced Entra token is the dangerous case: sent
        // as a password it comes back as "authentication failed", which reads
        // like a user error rather than a routing bug.
        const PG: &str = "postgres://u@h:5432/db";
        const MS: &str = "mssql://sa@h:1433/db";
        const TOKEN: &str = "eyJhbGciOi.ACCESS.TOKEN";
        let pg = ServerBackend::POSTGRES;
        let ms = ServerBackend::SQL_SERVER;

        // No credential: the URL is dialed exactly as it stands, and SQL
        // Server logs in with the URL's own user (MssqlAuth::Password).
        for (backend, url) in [(pg, PG), (ms, MS)] {
            let login = backend.login(url, ServerCredential::None).unwrap();
            assert_eq!(login.url, url);
            assert!(login.aad_token.is_none());
        }

        // A password is spliced into the URL on both engines.
        let login = pg.login(PG, ServerCredential::Password("p@ss")).unwrap();
        assert_eq!(login.url, "postgres://u:p%40ss@h:5432/db");
        assert!(login.aad_token.is_none());
        let login = ms.login(MS, ServerCredential::Password("p@ss")).unwrap();
        assert_eq!(login.url, "mssql://sa:p%40ss@h:1433/db");
        assert!(login.aad_token.is_none());

        // Postgres has no token login: the token goes in as the password.
        let login = pg
            .login(PG, ServerCredential::EntraToken(TOKEN.to_string()))
            .unwrap();
        assert_eq!(login.url, format!("postgres://u:{TOKEN}@h:5432/db"));
        assert!(
            login.aad_token.is_none(),
            "Postgres has no driver-side token login to route to"
        );

        // SQL Server does: the token goes to the driver and the URL is left
        // untouched — a token in the URL would be sent as a password.
        let login = ms
            .login(MS, ServerCredential::EntraToken(TOKEN.to_string()))
            .unwrap();
        assert_eq!(login.url, MS, "the URL must be unchanged");
        assert!(
            !login.url.contains(TOKEN),
            "the access token must never reach the URL on SQL Server"
        );
        assert_eq!(login.aad_token.as_deref(), Some(TOKEN));
    }

    #[test]
    fn a_login_reports_a_url_that_cannot_carry_the_secret() {
        // The splice is fallible, and its error is the one the connect
        // surfaces — it must not be swallowed into a connect attempt with a
        // secret-less URL.
        for backend in [ServerBackend::POSTGRES, ServerBackend::SQL_SERVER] {
            assert!(backend
                .login("not a url", ServerCredential::Password("p"))
                .is_err());
            // Nothing is spliced, so there is nothing to fail on: an unusable
            // URL stays the driver's error to report, exactly as before.
            assert!(backend.login("not a url", ServerCredential::None).is_ok());
        }
        // A Postgres token is spliced, so it fails here…
        assert!(ServerBackend::POSTGRES
            .login("not a url", ServerCredential::EntraToken("t".to_string()))
            .is_err());
        // …while a SQL Server token never touches the URL, which is the whole
        // point: nothing can fail at this step.
        assert!(ServerBackend::SQL_SERVER
            .login("not a url", ServerCredential::EntraToken("t".to_string()))
            .is_ok());
    }

    #[test]
    fn a_saved_entry_takes_the_backend_s_variant() {
        let tunnel = None;
        let pg = ServerBackend::POSTGRES.saved(
            "prod",
            "postgres://u@h:5432/db",
            tunnel.clone(),
            ServerAuth::Password,
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
            ServerAuth::Entra(EntraAuth::interactive_default()),
        );
        assert!(matches!(ms, SavedConnection::SqlServer { .. }));
        assert_eq!(ms.backend(), BackendKind::SqlServer);
        match ms {
            SavedConnection::SqlServer { auth, .. } => {
                assert!(
                    matches!(auth, ServerAuth::Entra(_)),
                    "auth mode is preserved"
                )
            }
            _ => unreachable!(),
        }
    }
}
