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

/// Where a tunnel passphrase came from. The two sources differ in what a
/// rejection means, which is why this is an enum rather than the "did we have
/// one" bool it replaced (FRE-151).
///
/// A **keyring** passphrase is one a tunnel open has already accepted, so a
/// rejection means the stored copy went stale and should go. A **session**
/// passphrase carries no such guarantee: [`AppState::stash_ssh_passphrase`]
/// writes the user's typed passphrase into session memory *unvalidated* —
/// that map is the only channel to this re-entrant call — so reading a session
/// hit as a stale stored secret let a single typo delete the saved passphrase.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PassphraseSource {
    /// Typed at the prompt this session. Never validated on its own.
    Session,
    /// Read back from the OS keyring, where only accepted passphrases land.
    Keyring,
}

/// Drops the passphrase that just failed out of session memory and reports
/// whether the keyring copy should go too.
///
/// Session memory is cleared for **both** sources: the value is wrong wherever
/// it came from, and leaving it would shadow the keyring on the next attempt.
/// Only a keyring-sourced rejection also deletes the stored copy — see
/// [`PassphraseSource`].
#[must_use = "the verdict decides whether the keyring copy is deleted; dropping it \
              leaves a stale passphrase that every later connect re-offers"]
pub(super) fn forget_failed_ssh_passphrase(
    session: &mut HashMap<String, String>,
    key: &str,
    source: PassphraseSource,
) -> bool {
    session.remove(key);
    source == PassphraseSource::Keyring
}

/// Records the user's "remember this passphrase" answer for `url` — ticked or
/// cleared — to be redeemed by [`redeem_ssh_remember`] once a tunnel open
/// accepts it (FRE-161).
///
/// Withdrawing matters as much as parking: the intent is keyed by locator and
/// outlives the attempt that parked it whenever that attempt *pauses* — on a
/// host-key prompt — or is abandoned. (A passphrase the tunnel rejects
/// withdraws it there and then.) Without the withdrawal, a box ticked once
/// would make every later passphrase for that connection persist, including
/// ones the user deliberately declined to save.
///
/// A declined choice is recorded as `false` rather than by removing the entry,
/// because "answered no" and "never asked" are different states and the
/// difference is user-visible: only the second may offer a ticked box on the
/// next prompt. They were the same state while this was a set, which quietly
/// unticked the re-prompt for a passphrase typed into the connection form —
/// that path stashes the passphrase without parking a choice at all.
pub(super) fn park_ssh_remember(pending: &mut HashMap<String, bool>, url: &str, remember: bool) {
    pending.insert(url.to_string(), remember);
}

/// Withdraws the parked choice for a passphrase a tunnel open rejected, and
/// reports what the re-prompt's "remember" box should offer.
///
/// The withdrawal is the one [`park_ssh_remember`] argues for: the choice was
/// made about the secret that just failed, so leaving it parked would let some
/// later attempt redeem it. Reporting what it *was* is what keeps the
/// re-prompt honest — the box used to be re-ticked on every re-prompt, so a
/// user who cleared it and then mistyped had that decision undone by the typo
/// (FRE-162).
///
/// An attempt with no answer on record defaults to offering the box ticked:
/// nothing has asked this session, so its prompt is a first prompt. That is
/// usually a passphrase read back from the keyring, or one typed into the
/// connection form, which stashes it without parking anything — but the rule
/// is the record, not the source. An answer left parked by an earlier attempt
/// is honoured whatever supplied the passphrase this time. That is why the
/// absence of an answer has to be distinguishable from an answer of "no", and
/// why [`park_ssh_remember`] records both.
#[must_use = "the verdict is the re-prompt's checkbox; dropping it re-ticks a box the \
              user deliberately cleared"]
pub(super) fn withdraw_ssh_remember(pending: &mut HashMap<String, bool>, url: &str) -> bool {
    pending.remove(url).unwrap_or(true)
}

/// Consumes any parked choice for `url` and reports whether the passphrase
/// that just opened a tunnel should be written to the keyring.
///
/// The intent is consumed whichever source supplied the passphrase, so it can
/// never outlive the attempt that redeemed it and surprise a later one. It is
/// only *acted* on for a session-sourced passphrase: a keyring-sourced one is
/// already stored, so re-storing it is a write whose only possible effect is
/// to fail — on every reconnect.
///
/// A free function over the map, like [`forget_failed_ssh_passphrase`] above,
/// so the policy can be executed by a test rather than read off the source:
/// this decides whether a secret reaches the OS keyring, and *both* polarities
/// are ways to get it wrong — one silently loses the user's choice, the other
/// stores a secret they declined.
#[must_use = "the verdict decides whether the passphrase is written to the keyring"]
pub(super) fn redeem_ssh_remember(
    pending: &mut HashMap<String, bool>,
    url: &str,
    source: Option<PassphraseSource>,
) -> bool {
    let parked = pending.remove(url).unwrap_or(false);
    parked && source == Some(PassphraseSource::Session)
}

/// Keyring key for a connection's cached Entra refresh token. Disjoint from the
/// password (bare URL) and SSH passphrase (`#ssh`) keys, so the three never
/// collide. Only a refresh token is ever cached here — never an access token.
pub(crate) fn entra_secret_key(url: &str) -> String {
    format!("{url}#entra")
}

/// Every key a connection's locator owns, in the keyring and in session
/// memory alike: the database password (the bare locator), the SSH key
/// passphrase, and the cached Entra refresh token.
///
/// One list rather than three call sites naming the same three keys, because
/// an edit migrates them and a removal deletes them, and a key left out of
/// either is a secret that outlives the connection it belonged to without
/// anything saying so.
pub(super) fn secret_keys(locator: &str) -> [String; 3] {
    [
        locator.to_string(),
        ssh_secret_key(locator),
        entra_secret_key(locator),
    ]
}

/// Carries a connection's in-memory secrets, and its parked "remember the
/// passphrase" choice, across an edit that moved the locator (FRE-162).
///
/// [`AppState::update_saved`] migrates the keyring entries for a reason that
/// applies just as much here: the locator is the key, so an edit that changes
/// host, port or database moves every session secret keyed by it. Left behind,
/// they are a passphrase and a password filed under a locator nothing will look
/// up again, while the connection they belong to re-prompts for secrets the app
/// is still holding.
///
/// **The parked "remember" choice deliberately does not move**, though it is
/// keyed by locator too, and FRE-162 was filed saying it should. An edit lands
/// only while the *new* locator is open ([`AppState::save_or_apply_edit`] is
/// reached through [`AppState::save_server_if_open`], which no-ops otherwise),
/// so a connect to it has already succeeded — in this call, or in an earlier
/// one when the form re-submits onto an already-open connection — and that
/// connect redeemed or recorded its own answer. Anything carried over can
/// therefore only be consumed by some *later* attempt — precisely what
/// [`park_ssh_remember`] exists to prevent, and a way to store a passphrase the
/// user declined: tick on the old locator, abandon the attempt at a host-key
/// card, edit the URL, untick at the new prompt, and the old answer would
/// overwrite the new one the successful connect had just consumed.
///
/// The abandoned attempt keeps its own answer under the old locator, where it
/// stays redeemable if that attempt ever resumes — its host-key card is still
/// on screen, and trusting the key reconnects to the old URL.
pub(super) fn carry_session_secrets(session: &mut HashMap<String, String>, old: &str, new: &str) {
    // No `old == new` guard: the only caller already checks it, and moving a
    // key onto itself is a no-op regardless — which
    // `an_unmoved_locator_and_unrelated_connections_are_untouched_by_an_edit`
    // executes rather than assumes.
    for (old_key, new_key) in secret_keys(old).into_iter().zip(secret_keys(new)) {
        let Some(secret) = session.remove(&old_key) else {
            continue;
        };
        // Never *over* a secret already under the new key: that one came from
        // the connect that just succeeded, and [`migrate_secret`] gives the
        // keyring copy the same precedence for the same reason. Overwriting it
        // would put an unvalidated secret where the next connect reads it as
        // previously accepted — and `forget_stale_secret` then deletes the
        // stored copy when it fails, which is FRE-151 reached through an edit.
        session.entry(new_key).or_insert(secret);
    }
}

/// Drops what a removed connection leaves behind in memory (FRE-162).
///
/// The keyring entries go with the entry itself; these are their session-lived
/// counterparts, and leaving them is not merely untidy. A parked intent
/// outliving its connection means recreating the same URL in the same session
/// writes back to the keyring the very passphrase the deletion removed.
///
/// The answer is *removed* rather than set to `false`, because those are
/// different states here too: a recreated connection has never been asked, so
/// its first prompt must come up ticked like any other
/// ([`withdraw_ssh_remember`] is what tells them apart).
pub(super) fn forget_connection_secrets(
    session: &mut HashMap<String, String>,
    pending: &mut HashMap<String, bool>,
    locator: &str,
) {
    for key in secret_keys(locator) {
        session.remove(&key);
    }
    pending.remove(locator);
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

/// How an already-saved server connection is configured to connect: its SSH
/// tunnel and its auth mode, or the no-tunnel/password defaults when hubro has
/// never seen this locator.
///
/// Exists for [`AppState::open_target`], and pure so that its consequence is
/// testable without a runtime: a connect *persists how it connected*, so
/// handing the connect flow anything other than what the entry already records
/// silently rewrites `connections.toml`. The saved locator and the normalized
/// URL a command line produces are the same string by construction — both come
/// out of `normalize_pg_url`/`normalize_mssql_url` — so this is an equality
/// match rather than a fuzzy one.
///
/// SQLite entries never match: they are keyed by path, have no URL, and reach
/// none of the server flows.
pub(super) fn saved_server_settings(
    saved: &[SavedConnection],
    url: &str,
) -> (Option<TunnelConfig>, ServerAuth) {
    saved
        .iter()
        .find_map(|entry| match entry {
            SavedConnection::Postgres {
                url: saved_url,
                tunnel,
                auth,
                ..
            }
            | SavedConnection::SqlServer {
                url: saved_url,
                tunnel,
                auth,
                ..
            } if saved_url == url => Some((tunnel.clone(), auth.clone())),
            _ => None,
        })
        .unwrap_or((None, ServerAuth::Password))
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

/// Carries a renamed group's fold across the rename (FRE-120).
///
/// The fold is remembered by name, in a *different file* from the group
/// itself: the group lives in connections.toml, the fold in session.toml. So
/// a rename that moved only the group would silently expand it and leave a
/// dead name behind to accumulate. Free functions over the plain `Vec` rather
/// than signal code inline, so the claim is a test rather than a comment.
///
/// The dedup at the end is not hypothetical bookkeeping: a rename onto a name
/// the list already holds (a fold left by an earlier group of that name)
/// would otherwise leave it twice, and every later `retain`/rename would have
/// to cope with that.
fn rename_collapsed(collapsed: &mut Vec<String>, old: &str, new: &str) {
    for name in collapsed.iter_mut() {
        if name == old {
            *name = new.to_string();
        }
    }
    let mut seen: Vec<String> = Vec::with_capacity(collapsed.len());
    collapsed.retain(|name| {
        let fresh = !seen.iter().any(|s| s == name);
        if fresh {
            seen.push(name.clone());
        }
        fresh
    });
}

/// Whether `old` is folded, i.e. whether [`rename_collapsed`] has anything to
/// do — kept separate so the caller can take the cheap read before the write.
fn collapsed_needs_rename(collapsed: &[String], old: &str) -> bool {
    collapsed.iter().any(|name| name == old)
}

/// Drops a deleted group's fold (FRE-120). A name nothing can expand again
/// would sit in session.toml forever, and would silently re-collapse a group
/// later created with the same name.
fn forget_collapsed(collapsed: &mut Vec<String>, name: &str) {
    collapsed.retain(|folded| folded != name);
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
    pub fn rename_saved_group(mut self, old: &str, new: &str) -> Result<String, GroupError> {
        let renamed = { self.saved.write().rename_group(old, new) };
        let Ok(new_name) = renamed else {
            return renamed;
        };
        self.persist_saved();
        if new_name != old && collapsed_needs_rename(&self.collapsed_groups.peek(), old) {
            rename_collapsed(&mut self.collapsed_groups.write(), old, &new_name);
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
            forget_collapsed(&mut self.collapsed_groups.write(), name);
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
            // Synchronously, and before the spawn: session memory is what the
            // *next* connect reads first, and the next connect can begin as
            // soon as this returns — the user is back on the connections
            // screen. Inside the spawned block it would instead land after
            // three keyring round-trips, and a reconnect in that window would
            // re-prompt for a secret the app is holding. The keyring migration
            // is spawned because it is I/O; this is a map write.
            carry_session_secrets(
                &mut self.session_passwords.write(),
                &old_locator,
                &new_locator,
            );
            spawn_forever(async move {
                for (old, new) in secret_keys(&old_locator)
                    .into_iter()
                    .zip(secret_keys(&new_locator))
                {
                    migrate_secret(old, new).await;
                }
            });
        }
    }

    /// Removes a saved connection and persists. Postgres and SQL Server
    /// entries also drop their keyring credentials (database password, SSH key
    /// passphrase, and cached Entra refresh token; deleting a missing entry is
    /// a no-op), and every entry drops the same keys from session memory along
    /// with any parked "remember the passphrase" choice (FRE-162).
    ///
    /// An open tab on the removed connection keeps working — nothing here
    /// touches the live connection — but it no longer has a remembered secret
    /// to reconnect with, which is the intended reading of a deletion rather
    /// than an oversight.
    pub fn remove_saved(mut self, locator: &str) {
        let removed = self.saved.write().remove(locator);
        if let Some(entry) = removed {
            // The in-memory stores are keyed by locator whatever the backend
            // is, and clearing them is not I/O, so it happens for every entry
            // and before the keyring work is spawned (FRE-162).
            forget_connection_secrets(
                &mut self.session_passwords.write(),
                &mut self.ssh_remember.write(),
                locator,
            );
            if let SavedConnection::Postgres { url, .. } | SavedConnection::SqlServer { url, .. } =
                entry
            {
                // Best-effort, off-thread: a missing keyring just means
                // nothing was stored.
                spawn_forever(async move {
                    for key in secret_keys(&url) {
                        let _ = crate::secrets::delete_password_async(key).await;
                    }
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

    /// Opens a database named on the command line or handed over by the OS
    /// (FRE-114) — one entry point for `hubro <file>`, `hubro <url>`, and a
    /// file association on all three platforms.
    ///
    /// Drives the ordinary connect flows rather than a shortcut of its own,
    /// for the reason session restore does: an argv-opened tab should be
    /// indistinguishable from one opened by hand, prompts and all. Which also
    /// settles the two things the two kinds of target do differently, because
    /// both are what the existing flows already do:
    ///
    /// - a **file** is opened without joining the saved list, because
    ///   [`Self::connect`] never saves — "opened once from a shell" is not a
    ///   connection anybody asked to keep;
    /// - a **server** does join it on success, because every server connect
    ///   does ([`Self::save_server_if_open`]). What is saved is the normalized
    ///   locator, which carries no password.
    ///
    /// A URL that names an **already-saved** connection is connected the way
    /// that connection is saved — its tunnel and its auth mode — rather than
    /// as a bare URL. Partly because that is what someone typing the URL of
    /// their tunneled connection means, and partly because the alternative
    /// destroys it: a connect persists how it connected
    /// ([`SavedList::add`](crate::config::SavedList::add) adopts the tunnel
    /// and auth it is handed, in *both* directions), so connecting with
    /// `None`/`Password` would erase a saved SSH tunnel and downgrade a saved
    /// Entra sign-in to a password, on disk, with nothing shown to the user.
    /// It does not even need the connect to succeed: `focus_or_reserve`
    /// short-circuits when the tab is already open — which a session restore
    /// has just made likely — and saves on the way out.
    ///
    /// Any password from the URL goes into session memory only — never the
    /// keyring. Storing a credential permanently is a decision the "remember
    /// password" checkbox exists to take; a password that happened to be on a
    /// command line has not been through it.
    pub async fn open_target(self, target: OpenTarget) {
        match target {
            OpenTarget::File(path) => self.connect(path).await,
            OpenTarget::Server {
                backend,
                url,
                password,
            } => {
                let name = crate::cli::display_name(&url);
                let backend = ServerBackend::of(backend);
                // Scoped so the read guard is dropped before the awaits below.
                let (tunnel, auth) = {
                    let saved = self.saved.read();
                    saved_server_settings(saved.entries(), &url)
                };
                match password {
                    // A password off the command line takes the same route as
                    // one just typed into the prompt, and for the same reason:
                    // nothing has validated it. Putting it into session memory
                    // instead — where every other entry is a secret a connect
                    // already accepted — makes a typo indistinguishable from a
                    // stored password that has gone stale, and
                    // [`Self::connect_server`] answers that by *deleting* the
                    // keyring entry for the locator. A mistyped `hubro
                    // postgres://u:typ0@host/db` would then destroy the saved
                    // credential of the connection it names.
                    //
                    // This path stores nothing until the connect succeeds, and
                    // never forgets anything when it fails. `remember: false`:
                    // a password that happened to be on a command line has not
                    // been through the "remember password" decision, so it
                    // stays out of the keyring either way.
                    //
                    // Only for a password-authenticated connection, though:
                    // this flow saves with `ServerAuth::Password`, which for an
                    // Entra-saved entry would be the same downgrade by another
                    // route. An Entra connection signs in with a token and has
                    // no use for a password anyway, so it connects as saved.
                    Some(password) if matches!(auth, ServerAuth::Password) => {
                        self.connect_server_with_password(
                            backend, url, name, password, false, tunnel,
                        )
                        .await
                    }
                    _ => self.connect_server(backend, url, name, tunnel, auth).await,
                }
            }
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
        //
        // **Both sources hold only secrets a connect already accepted**, and
        // the `had_password` branch below relies on it: a rejection there is
        // read as "the remembered password went stale" and answered by
        // deleting it from the keyring. Anything that writes an *unvalidated*
        // password into `session_passwords` turns a typo into the silent
        // destruction of a saved credential, so a password the user has just
        // supplied belongs in [`Self::connect_server_with_password`], which
        // remembers it only once it has worked (see [`Self::open_target`]).
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
                    // Nothing parks a database-password choice — it is acted
                    // on the moment the connect succeeds — so there is no
                    // earlier answer to carry and this prompt asks fresh.
                    remember: true,
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
    /// backend and auth mode the prompt carried.
    ///
    /// Keyring persistence does **not** happen here. The "remember" choice is
    /// parked for [`Self::open_tunnel`] to redeem at the moment a tunnel open
    /// accepts the passphrase, which is both the FRE-151 condition and — since
    /// the connect frequently resumes elsewhere, via a host-key or password
    /// prompt — the only point that reliably happens (FRE-161).
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
        // Park the choice for [`Self::open_tunnel`] to redeem once a tunnel
        // open has actually accepted this passphrase (FRE-161).
        //
        // It used to be acted on right here, once the connect returned, gated
        // on the connection being open. That gate is wrong in both directions:
        // the connect has frequently not *finished* at this point — an
        // untrusted host key, a database password prompt or an Entra sign-in
        // each park it and resume it from somewhere else — while the
        // passphrase may already have been accepted by then. Trusting a host
        // key and connecting therefore stored nothing and said nothing, and
        // session memory kept the passphrase, so the loss only showed up as a
        // re-prompt after the next restart.
        //
        // Set *or cleared*, so an unticked box on a later attempt is not
        // ignored because an earlier one ticked it.
        self.set_ssh_remember(&url, remember);
        self.password_prompt.set(None);
        self.connect_server(
            ServerBackend::of(backend),
            url.clone(),
            name,
            Some(tunnel),
            auth,
        )
        .await;
    }

    /// Records — or withdraws — the user's "remember this SSH passphrase"
    /// choice for `url`, pending a tunnel open that accepts it. The policy
    /// itself is [`park_ssh_remember`], where a test can execute it.
    fn set_ssh_remember(mut self, url: &str, remember: bool) {
        park_ssh_remember(&mut self.ssh_remember.write(), url, remember);
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
        // Value and source travel as one: which of the two supplied it decides
        // what a rejection below means, and two separate `Option`s kept in step
        // by hand would let a passphrase with no recorded source silently skip
        // that decision (FRE-151).
        let mut stored: Option<(String, PassphraseSource)> = None;
        if matches!(config.auth, TunnelAuth::KeyFile { .. }) {
            // Read into a local first: the guard is a statement temporary, so
            // nothing is held across the keyring await below.
            let session = self.session_passwords.read().get(&secret_key).cloned();
            stored = match session {
                Some(value) => Some((value, PassphraseSource::Session)),
                None => crate::secrets::get_password_async(secret_key.clone())
                    .await
                    .ok()
                    .flatten()
                    .map(|value| (value, PassphraseSource::Keyring)),
            };
        }
        let (passphrase, source) = stored.unzip();
        let target = match (backend.url_target)(url) {
            Ok(target) => target,
            Err(err) => {
                self.fail_connect(url, err.to_string());
                return None;
            }
        };
        let known_hosts = crate::tunnel::default_known_hosts_read();
        match Tunnel::open(config.clone(), passphrase, target.0, target.1, &known_hosts).await {
            Ok(live) => {
                // The tunnel opened, so the passphrase it was handed is good
                // as far as anything here can tell: the key loaded and the
                // session it authenticated came up. Note what that is *not* —
                // the server never sees this passphrase (it decrypts a local
                // key file), and for an unencrypted key it was ignored
                // outright. It is the condition FRE-151
                // places on writing it to the keyring, and the reason the
                // write lives here rather than wherever the connect eventually
                // ends (FRE-161).
                //
                // Only for a session-sourced one: a keyring-sourced passphrase
                // is already stored, and re-storing it would be a write whose
                // only possible effect is to fail.
                // The write guard is a statement temporary: nothing is held
                // across the keyring await below.
                let redeem = redeem_ssh_remember(&mut self.ssh_remember.write(), url, source);
                if redeem {
                    self.persist_ssh_passphrase(url).await;
                }
                match (backend.via_local_port)(url, live.local_port()) {
                    Ok(rewritten) => Some((rewritten, Some(live))),
                    Err(err) => {
                        self.fail_connect(url, err.to_string());
                        None
                    }
                }
            }
            Err(err @ TunnelError::NeedsPassphrase(_)) => {
                self.release_connect(url);
                // The choice was made for the passphrase that just failed, so
                // it dies with it: the re-prompt asks again, and leaving it
                // parked would let some later attempt redeem a choice the user
                // made about a different secret. What it *was* becomes the
                // re-prompt's default, so a typo cannot re-tick a box the user
                // cleared (FRE-162) — one call, because reading the answer
                // after withdrawing it would always read the default.
                let remember = withdraw_ssh_remember(&mut self.ssh_remember.write(), url);
                if let Some(source) = source {
                    // Something was tried and rejected. It leaves session
                    // memory either way; only a keyring-sourced one is stale
                    // enough to delete (FRE-151). The write guard is a
                    // statement temporary — nothing is held across the await.
                    let drop_stored = forget_failed_ssh_passphrase(
                        &mut self.session_passwords.write(),
                        &secret_key,
                        source,
                    );
                    if drop_stored {
                        let _ = crate::secrets::delete_password_async(secret_key).await;
                    }
                    self.connect_error.set(Some(err.to_string()));
                }
                self.password_prompt.set(Some(PasswordPrompt {
                    url: url.to_string(),
                    name: name.to_string(),
                    kind: PromptKind::SshPassphrase,
                    backend: backend.kind,
                    tunnel: Some(config.clone()),
                    auth: auth.clone(),
                    remember,
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

    /// The body of one `impl AppState` method, by brace matching from its
    /// signature.
    fn method_body(source: &str, signature: &str) -> String {
        let from = source
            .find(signature)
            .unwrap_or_else(|| panic!("no method `{signature}` in connect.rs"));
        let open = source[from..]
            .find('{')
            .expect("a method signature is followed by a body");
        let mut depth = 0usize;
        for (offset, ch) in source[from + open..].char_indices() {
            match ch {
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        return source[from..from + open + offset + 1].to_string();
                    }
                }
                _ => {}
            }
        }
        panic!("unbalanced braces after `{signature}`");
    }

    /// The source of [`AppState::open_target`].
    ///
    /// Read rather than executed because the method needs a Dioxus runtime, a
    /// live server and a keyring to reach its consequences, while every
    /// regression it has actually had was a *routing* mistake — one visible
    /// right here. The properties it is asked about are pinned beside the
    /// behavioural tests of the values themselves, so neither stands alone: the
    /// value tests would pass against a path that ignores them, and these would
    /// pass against a path that routes correctly to something broken.
    fn open_target_body() -> String {
        method_body(
            include_str!("connect.rs"),
            "pub async fn open_target(self, target: OpenTarget)",
        )
    }

    /// The source of [`AppState::open_tunnel`], read for the reason
    /// [`open_target_body`] documents: reaching the branch it is asked about
    /// needs an SSH server, a keyring and a Dioxus runtime, and the bug there
    /// was a routing mistake visible right here.
    fn open_tunnel_body() -> String {
        method_body(include_str!("connect.rs"), "async fn open_tunnel(")
    }

    /// A saved Postgres entry with an SSH tunnel and interactive Entra auth —
    /// the two fields `SavedList::add` adopts, and so the two an argv connect
    /// can destroy.
    fn saved_with_tunnel_and_entra(url: &str) -> SavedConnection {
        SavedConnection::Postgres {
            name: "prod".into(),
            url: url.into(),
            tunnel: Some(TunnelConfig {
                host: "bastion.example.com".into(),
                port: 22,
                user: "jump".into(),
                auth: TunnelAuth::Agent,
            }),
            auth: ServerAuth::Entra(EntraAuth::interactive_default()),
            protection: WriteProtection::Open,
            color: None,
            group: None,
        }
    }

    #[test]
    fn a_saved_connection_lends_the_command_line_its_tunnel_and_auth() {
        const URL: &str = "postgres://u@h:5432/db";
        let saved = vec![saved_with_tunnel_and_entra(URL)];

        let (tunnel, auth) = saved_server_settings(&saved, URL);
        assert_eq!(tunnel.as_ref().unwrap().host, "bastion.example.com");
        assert!(matches!(auth, ServerAuth::Entra(_)));

        // A locator hubro has never seen keeps the bare-URL defaults: there is
        // nothing to preserve, and inventing a tunnel would be worse.
        let (tunnel, auth) = saved_server_settings(&saved, "postgres://u@other:5432/db");
        assert!(tunnel.is_none());
        assert!(matches!(auth, ServerAuth::Password));

        // A SQLite entry is keyed by path and reaches no server flow, so it can
        // never be mistaken for a match.
        let sqlite = vec![SavedConnection::Sqlite {
            name: "app.db".into(),
            path: PathBuf::from(URL),
            protection: WriteProtection::Open,
            color: None,
            group: None,
        }];
        let (tunnel, auth) = saved_server_settings(&sqlite, URL);
        assert!(tunnel.is_none());
        assert!(matches!(auth, ServerAuth::Password));
    }

    #[test]
    fn an_argv_connect_to_a_saved_connection_rewrites_nothing_on_disk() {
        // The consequence that makes the lookup load-bearing rather than a
        // nicety. A connect persists how it connected, and `SavedList::add`
        // adopts the tunnel and auth it is handed in *both* directions — so
        // connecting a saved, tunneled, Entra connection as a bare URL erases
        // its tunnel and downgrades its sign-in, on disk, silently. This walks
        // the same values `open_target` now passes through the same `add` the
        // save path calls, and asserts the entry does not change.
        const URL: &str = "postgres://u@h:5432/db";
        let dir = tempfile::tempdir().unwrap();
        let (mut list, _) = SavedList::load(&dir.path().join("connections.toml"));
        assert!(list.add(saved_with_tunnel_and_entra(URL)));

        let (tunnel, auth) = saved_server_settings(list.entries(), URL);
        let would_save = ServerBackend::POSTGRES.saved("cli-derived-name", URL, tunnel, auth);
        assert!(
            !list.add(would_save),
            "an argv connect to a saved connection must leave the entry alone — \
             a `true` here is a rewrite of connections.toml the user never asked \
             for and never sees"
        );

        match &list.entries()[0] {
            SavedConnection::Postgres {
                name, tunnel, auth, ..
            } => {
                assert_eq!(tunnel.as_ref().unwrap().host, "bastion.example.com");
                assert!(matches!(auth, ServerAuth::Entra(_)));
                // The name was never at risk (`add` keeps it), but a rewrite
                // that took it would be just as invisible.
                assert_eq!(name, "prod");
            }
            other => panic!("unexpected entry {other:?}"),
        }

        // Everything above proves the *values* are safe once `open_target`
        // looks them up. It does not prove `open_target` looks them up — and a
        // test that walks the helper by hand stays green while the argv path
        // goes back to passing `None`/`Password`, which is precisely the
        // regression this exists to catch. So the wiring is pinned here too,
        // by the same source-level instrument the test below uses.
        let body = open_target_body();
        assert!(
            body.contains("saved_server_settings(saved.entries(), &url)"),
            "open_target no longer looks the saved entry up, so it is back to \
             handing the connect defaults — which erases a saved tunnel and \
             downgrades a saved Entra sign-in on disk"
        );
        assert!(
            body.contains("self.connect_server(backend, url, name, tunnel, auth)"),
            "the looked-up tunnel and auth must be the ones handed to the connect"
        );
        assert!(
            body.contains("backend, url, name, password, false, tunnel,"),
            "the password route must carry the looked-up tunnel too"
        );
        assert!(
            body.contains("matches!(auth, ServerAuth::Password)"),
            "without this guard an argv password takes the password route for an \
             Entra-saved connection, and that route saves ServerAuth::Password — \
             the same downgrade by another door"
        );
    }

    #[test]
    fn a_command_line_password_never_enters_session_memory() {
        // `session_passwords` holds secrets a connect has *accepted*, and
        // `connect_server` acts on that: when a password read from there (or
        // from the keyring) is rejected, it concludes the stored secret went
        // stale and deletes the keyring entry for that locator. Writing an
        // unvalidated password there therefore turns a typo into the silent
        // destruction of a saved credential — `hubro postgres://u:typ0@host/db`
        // wiping the password of the saved connection with the same URL.
        //
        // The fix is a routing decision, and this checks that decision at its
        // source: the failure itself needs a live server, a keyring and a
        // Dioxus runtime (`AppState::new` must be called from a component), so
        // it is not reachable from a unit test — but the routing is, and the
        // routing is where the bug was.
        let body = open_target_body();
        assert!(
            !body.contains("session_passwords"),
            "open_target writes session memory again — an unvalidated password \
             there makes a mistyped command line delete a saved keyring password"
        );
        assert!(
            body.contains("connect_server_with_password"),
            "a command-line password must take the same route as one typed into \
             the prompt: tried now, remembered only if it works, and never \
             mistaken for a stored secret"
        );
        // The password must still be *used* — routing it nowhere would also
        // pass the assertions above.
        assert!(
            body.contains("Some(password)"),
            "the password that came with the URL is no longer being connected with"
        );
    }

    /// The source of [`AppState::connect_server_with_ssh_passphrase`], read
    /// for the reason [`open_target_body`] documents.
    fn ssh_passphrase_body() -> String {
        method_body(
            include_str!("connect.rs"),
            "pub async fn connect_server_with_ssh_passphrase(",
        )
    }

    #[test]
    fn the_remember_choice_is_redeemed_where_the_passphrase_is_accepted() {
        // FRE-161. The choice is made at the prompt, but FRE-151 permits the
        // keyring write only once a tunnel open has *accepted* the passphrase.
        // Those are different moments — and, the part that was wrong here,
        // frequently different connects: an untrusted host key, a database
        // password prompt or an Entra sign-in each park the attempt and finish
        // it from somewhere else.
        //
        // So the prompt parks an intent and `open_tunnel` redeems it. Putting
        // the write back at the prompt re-breaks FRE-151; gating it on the
        // *connection* being open re-breaks FRE-161.
        let body = ssh_passphrase_body();
        assert!(
            body.contains("set_ssh_remember(&url, remember)"),
            "the remember choice is dropped, or parked as something other than \
             what the user actually chose: {body}"
        );
        assert!(
            !body.contains("persist_ssh_passphrase"),
            "the prompt writes the passphrase itself, so one the server never \
             accepted can reach the keyring — which `open_tunnel` then reads \
             as previously validated and deletes on rejection (FRE-151): {body}"
        );
        assert!(
            !body.contains("open_locators"),
            "the prompt is gating on the connection being open again. A \
             connect parked on a host key or a password prompt has not opened \
             yet, which is exactly how the choice went missing (FRE-161): \
             {body}"
        );

        // And the redemption sits in the arm that means "accepted".
        let body = open_tunnel_body();
        let accepted = body
            .find("Ok(live)")
            .expect("open_tunnel must still have a success arm");
        let redeem = body
            .find("ssh_remember")
            .expect("open_tunnel must redeem the parked choice");
        let persist = body
            .find("persist_ssh_passphrase")
            .expect("...and act on it");
        let rejected = body
            .find("TunnelError::NeedsPassphrase")
            .expect("open_tunnel must still handle a rejected passphrase");
        assert!(
            accepted < redeem && redeem < persist && persist < rejected,
            "the passphrase is stored outside the arm where the tunnel \
             accepted it: {body}"
        );
        // The decision must go through `redeem_ssh_remember`, and be handed
        // the source rather than deciding without it. *What* it decides — the
        // polarity, the source rule, the consumption — is executed directly by
        // `a_parked_choice_decides_whether_the_passphrase_is_stored` and
        // `a_redeemed_choice_is_consumed_and_never_rewrites_the_keyring`,
        // because an inversion here moves no call and this test would not see
        // it.
        // Sliced past the comments before matching: the prose here says
        // "session-sourced" and "keyring-sourced", which satisfies a bare
        // search for `source` even when the call has hard-coded a source and
        // stopped consulting the one it was handed.
        let redemption = &body[accepted..persist];
        let redemption_code: String = redemption
            .lines()
            .filter(|line| !line.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            redemption_code.contains("redeem_ssh_remember(") && redemption_code.contains("source)"),
            "the redemption bypasses the policy, or hard-codes where the \
             passphrase came from instead of passing what it was handed: \
             {redemption_code}"
        );
        // And a rejected passphrase takes the choice with it — while handing
        // it to the re-prompt, which is FRE-162. Both halves are one call, so
        // this pins the call and
        // `a_re_prompt_offers_the_choice_the_user_already_made` executes what
        // it decides.
        let rejection = &body[rejected..];
        // The binding as written, not merely the call: `!withdraw_ssh_remember(…)`
        // satisfies a search for the call and leaves the `remember,` field
        // below untouched, so the answer inverts one line above everything
        // else this test reads. That is exactly how it got through.
        assert!(
            rejection.contains("let remember = withdraw_ssh_remember("),
            "a rejected passphrase leaves its remember choice parked, or the \
             withdrawn answer is negated on the way to the prompt — which \
             re-ticks a box the user cleared and ends in a keyring write of a \
             passphrase they declined: {rejection}"
        );
        // The field as written too: `remember: !remember,` contains
        // `remember,` and inverts the same decision one line lower.
        //
        // Matched a line at a time, not against a literal containing `\n`:
        // that is the FRE-160 trap, and `tests/line_endings.rs` records that
        // this file does not depend on the checkout's line endings.
        assert!(
            rejection.lines().any(|line| line.trim() == "remember,"),
            "the withdrawn choice is not reaching the re-prompt as written, so \
             the box is re-ticked after every typo, or offers the opposite of \
             what the user chose (FRE-162): {rejection}"
        );
        // And negation in any *other* form: a second binding
        // (`let remember = !remember;`) inverts the same decision while
        // leaving both the call and the field shorthand above intact. Read
        // over code only — the prose here discusses what is deliberately not
        // stored. Renaming the binding fails these assertions on purpose: the
        // field is passed as shorthand, so the name is load-bearing already,
        // and the looser match this replaced is what let the inversion past
        // five rounds of review.
        let rejection_code: String = rejection
            .lines()
            .filter(|line| !line.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            !rejection_code.contains("!remember") && !rejection_code.contains("!withdraw"),
            "the withdrawn answer is negated before it reaches the prompt: \
             {rejection_code}"
        );
        // And the write must sit *inside* the verdict's gate, the way
        // `open_tunnel_decides_on_the_passphrase_s_source_not_its_presence`
        // pins the delete against `if drop_stored`. Of the two keyring
        // decisions this function makes, that one was pinned and this one was
        // not — and an inverted `if redeem` is the worse half: the delete gate
        // inverted destroys a saved secret, while this one stores the
        // passphrase precisely when the user declined it and never when they
        // asked. Every other assertion here is positional or presence-based,
        // and inverting a gate moves nothing.
        let gate = method_body(&body, "if redeem {");
        assert!(
            gate.contains("persist_ssh_passphrase(url)"),
            "the keyring write is not conditional on the redeemed choice, so \
             the consent this whole mechanism exists to carry decides nothing \
             (FRE-161): {gate}"
        );
        assert_eq!(
            body.matches("persist_ssh_passphrase").count(),
            1,
            "a second, ungated write would store the passphrase whatever the \
             user chose, leaving the gate above inert"
        );
    }

    #[test]
    fn a_parked_choice_decides_whether_the_passphrase_is_stored() {
        // The policy itself, executed rather than read. The tests around it
        // pin *where* the write happens; these pin *what it decides*, which is
        // the half that can be inverted without moving a single call.
        //
        // Both polarities are defects, and the second is the worse one: losing
        // the choice is FRE-161 again, but storing a secret the user declined
        // is a privacy failure the app never mentions.
        let url = "postgres://u@h:5432/db";
        let mut pending = HashMap::new();

        // Ticked, then accepted from a passphrase just typed: stored.
        park_ssh_remember(&mut pending, url, true);
        assert!(redeem_ssh_remember(
            &mut pending,
            url,
            Some(PassphraseSource::Session)
        ));

        // Unticked: never stored, however the passphrase was accepted.
        park_ssh_remember(&mut pending, url, false);
        assert!(!redeem_ssh_remember(
            &mut pending,
            url,
            Some(PassphraseSource::Session)
        ));

        // An earlier tick must not survive a later untick — the intent is
        // keyed by locator and outlives the attempt that made it.
        park_ssh_remember(&mut pending, url, true);
        park_ssh_remember(&mut pending, url, false);
        assert!(!redeem_ssh_remember(
            &mut pending,
            url,
            Some(PassphraseSource::Session)
        ));

        // Never stored with no choice parked at all.
        assert!(!redeem_ssh_remember(
            &mut HashMap::new(),
            url,
            Some(PassphraseSource::Session)
        ));
    }

    #[test]
    fn a_redeemed_choice_is_consumed_and_never_rewrites_the_keyring() {
        let url = "postgres://u@h:5432/db";

        // One tick, one write: the next acceptance must not store again.
        let mut pending = HashMap::new();
        park_ssh_remember(&mut pending, url, true);
        assert!(redeem_ssh_remember(
            &mut pending,
            url,
            Some(PassphraseSource::Session)
        ));
        assert!(
            !redeem_ssh_remember(&mut pending, url, Some(PassphraseSource::Session)),
            "the parked choice survived the write it authorised, so every \
             later reconnect stores again"
        );

        // A keyring-sourced passphrase is already stored, so it is never
        // re-written — but the intent is still consumed, so it cannot lie in
        // wait for an unrelated later attempt.
        let mut pending = HashMap::new();
        park_ssh_remember(&mut pending, url, true);
        assert!(!redeem_ssh_remember(
            &mut pending,
            url,
            Some(PassphraseSource::Keyring)
        ));
        assert!(
            pending.is_empty(),
            "a choice redeemed against a stored passphrase stayed parked"
        );

        // No passphrase at all (agent auth, or an unencrypted key) stores
        // nothing and leaves nothing behind either.
        let mut pending = HashMap::new();
        park_ssh_remember(&mut pending, url, true);
        assert!(!redeem_ssh_remember(&mut pending, url, None));
        assert!(pending.is_empty());

        // Other locators are untouched throughout.
        let mut pending = HashMap::new();
        park_ssh_remember(&mut pending, url, true);
        park_ssh_remember(&mut pending, "postgres://u@other:5432/db", true);
        assert!(redeem_ssh_remember(
            &mut pending,
            url,
            Some(PassphraseSource::Session)
        ));
        assert_eq!(pending.len(), 1, "redeeming one locator cleared another");
    }
    /// The three locator-keyed keys, as a connection's secrets would sit in
    /// session memory: a database password, an SSH passphrase, an Entra token.
    ///
    /// Nothing writes the `#entra` one into session memory today — refresh
    /// tokens live in the keyring only. It is here because the point of
    /// [`secret_keys`] is that these paths walk the *list* rather than the
    /// keys they happen to know about, so the fixture holds a value under
    /// every key the list names.
    fn session_for(locator: &str) -> HashMap<String, String> {
        HashMap::from([
            (locator.to_string(), "db-password".to_string()),
            (ssh_secret_key(locator), "letmein".to_string()),
            (entra_secret_key(locator), "refresh-token".to_string()),
        ])
    }

    #[test]
    fn an_edited_locator_carries_its_session_secrets_with_it() {
        // FRE-162. The keyring entries already move — `update_saved` migrates
        // all three — but the session copies keyed by the same locator did
        // not, so the edited connection re-prompted for secrets the app was
        // still holding and the old ones sat under a locator naming nothing.
        const OLD: &str = "postgres://u@old:5432/db";
        const NEW: &str = "postgres://u@new:5432/db";
        let mut session = session_for(OLD);

        carry_session_secrets(&mut session, OLD, NEW);

        assert_eq!(
            session
                .get(ssh_secret_key(NEW).as_str())
                .map(String::as_str),
            Some("letmein"),
            "the session passphrase stayed under the old locator, so the edited \
             connection re-prompts for a secret the app is still holding"
        );
        assert_eq!(
            session.get(NEW).map(String::as_str),
            Some("db-password"),
            "the session database password stayed behind — the keyring copy \
             migrates, so this one must too or the two disagree"
        );
        assert_eq!(
            session
                .get(entra_secret_key(NEW).as_str())
                .map(String::as_str),
            Some("refresh-token")
        );
        // Nothing may be left under the old locator: it names no connection
        // now, and a later connection reusing that URL would inherit it.
        for key in secret_keys(OLD) {
            assert!(
                !session.contains_key(&key),
                "{key} survived the edit under the old locator"
            );
        }
    }

    #[test]
    fn an_edit_never_overwrites_a_secret_the_successful_connect_just_wrote() {
        // The edit is applied *after* the connect that confirms it, and that
        // connect files its validated password under the NEW locator. Carrying
        // the old value over the top of it would replace a secret the server
        // accepted with one it never saw — and the next connect reads a
        // session hit as good, fails, and `forget_stale_secret` deletes the
        // keyring copy: FRE-151, reached through an edit.
        //
        // `migrate_secret` gives the keyring copy the same precedence, in so
        // many words, so this is also the two stores agreeing rather than
        // drifting.
        const OLD: &str = "postgres://u@old:5432/db";
        const NEW: &str = "postgres://u@new:5432/db";
        // Every key, not just the bare locator: the passphrase and the Entra
        // token are filed under the same moved locator and reach the same
        // "read a session hit as already accepted" path, so a rule that held
        // for the password alone would be three-quarters of a fix.
        let mut session = session_for(OLD);
        for (index, key) in secret_keys(NEW).into_iter().enumerate() {
            session.insert(key, format!("validated-just-now-{index}"));
        }

        carry_session_secrets(&mut session, OLD, NEW);

        for (index, key) in secret_keys(NEW).into_iter().enumerate() {
            assert_eq!(
                session.get(&key).map(String::as_str),
                Some(format!("validated-just-now-{index}").as_str()),
                "the edit overwrote {key}, which the successful connect had \
                 just validated, with the pre-edit value"
            );
        }
        // The old copies still go: they are redundant, and leaving them lets a
        // later connection reusing that URL inherit them.
        for key in secret_keys(OLD) {
            assert!(!session.contains_key(&key), "{key} survived the edit");
        }

        // A key with nothing under the new locator still moves — otherwise
        // "never overwrite" could be satisfied by never carrying anything.
        let mut session = session_for(OLD);
        session.insert(NEW.to_string(), "validated-just-now".to_string());
        carry_session_secrets(&mut session, OLD, NEW);
        assert_eq!(
            session
                .get(ssh_secret_key(NEW).as_str())
                .map(String::as_str),
            Some("letmein")
        );
        assert_eq!(
            session.get(NEW).map(String::as_str),
            Some("validated-just-now")
        );
    }

    #[test]
    fn an_unmoved_locator_and_unrelated_connections_are_untouched_by_an_edit() {
        // An edit that changes only the display name keeps the locator, and
        // must not disturb what is filed under it.
        const URL: &str = "postgres://u@h:5432/db";
        let mut session = session_for(URL);
        carry_session_secrets(&mut session, URL, URL);
        assert_eq!(session, session_for(URL), "a no-op edit lost a secret");

        // A different connection's secrets are never collateral.
        const OTHER: &str = "postgres://u@other:5432/db";
        let mut session = session_for(OTHER);
        carry_session_secrets(&mut session, URL, "postgres://u@new:5432/db");
        assert_eq!(session, session_for(OTHER));
    }

    #[test]
    fn a_first_prompt_still_offers_to_remember() {
        // Carrying an earlier answer must not quietly change what happens when
        // there is no earlier answer. The SSH re-prompt's default is executed
        // by `a_re_prompt_offers_the_choice_the_user_already_made`; the
        // database-password prompt has no policy behind it at all — nothing
        // parks that choice — so its default is a literal, and a literal can
        // be flipped without moving a call or failing any policy test.
        let body = method_body(include_str!("connect.rs"), "pub async fn connect_server(");
        assert!(
            body.contains("remember: true"),
            "the password prompt no longer offers to remember by default, so \
             every saved connection quietly starts re-asking: {body}"
        );
    }

    #[test]
    fn a_removed_connection_leaves_nothing_behind_in_memory() {
        // FRE-162. `remove_saved` deletes the three keyring entries; their
        // session-lived counterparts stayed. The parked intent is again the
        // sharp edge — recreating the same URL in the same session would
        // redeem it and write back the passphrase the deletion just removed.
        const URL: &str = "postgres://u@h:5432/db";
        let mut session = session_for(URL);
        let mut pending = HashMap::new();
        park_ssh_remember(&mut pending, URL, true);

        forget_connection_secrets(&mut session, &mut pending, URL);

        assert!(
            !redeem_ssh_remember(&mut pending, URL, Some(PassphraseSource::Session)),
            "the deleted connection's remember choice is still parked, so \
             recreating the URL re-stores the passphrase the user deleted"
        );
        // Probed through `withdraw` as well, because `redeem` cannot tell a
        // cleared entry from one left behind as `false` — and that difference
        // is user-visible: a recreated connection has never been asked, so its
        // first prompt must come up ticked.
        let mut pending = HashMap::new();
        park_ssh_remember(&mut pending, URL, true);
        forget_connection_secrets(&mut session_for(URL), &mut pending, URL);
        assert!(
            withdraw_ssh_remember(&mut pending, URL),
            "the deleted connection left an answer behind, so recreating it \
             offers an unticked box the user never cleared"
        );
        assert!(pending.is_empty(), "the deletion left an entry behind");

        let mut session = session_for(URL);
        let mut pending = HashMap::new();
        park_ssh_remember(&mut pending, URL, true);
        forget_connection_secrets(&mut session, &mut pending, URL);
        for key in secret_keys(URL) {
            assert!(
                !session.contains_key(&key),
                "{key} outlived the connection it belonged to"
            );
        }

        // Only that connection's.
        const OTHER: &str = "postgres://u@other:5432/db";
        let mut session = session_for(OTHER);
        let mut pending = HashMap::new();
        park_ssh_remember(&mut pending, OTHER, true);
        forget_connection_secrets(&mut session, &mut pending, URL);
        assert_eq!(session, session_for(OTHER));
        assert!(pending.contains_key(OTHER));
    }

    #[test]
    fn connection_management_routes_every_locator_keyed_store_through_the_policy() {
        // The tests above execute what the policy decides; this pins that the
        // two paths still consult it. They are separable in the way that made
        // FRE-162 possible in the first place: `update_saved` and
        // `remove_saved` handled the keyring keys correctly for a year while
        // walking straight past the two in-memory stores keyed by the same
        // locator, and a policy nobody calls stays green forever.
        //
        // `AppState::new` needs a Dioxus runtime, so the methods themselves
        // are out of reach of a unit test — the call is what is checkable
        // here, and the call is what went missing.
        let update = method_body(
            include_str!("connect.rs"),
            "pub fn update_saved(mut self, old_locator: String, connection: SavedConnection) {",
        );
        assert!(
            update.contains("carry_session_secrets("),
            "an edit migrates the keyring entries but strands the session \
             passphrase and password under the old locator (FRE-162): {update}"
        );
        assert!(
            update.contains("session_passwords"),
            "the carry is not being handed session memory: {update}"
        );
        // And it must not reach for the parked choice: carrying that across is
        // how an abandoned attempt's tick comes to be redeemed by a later one.
        assert!(
            !update.contains("ssh_remember"),
            "an edit is moving the parked remember choice onto the new locator, \
             where a later reconnect redeems a choice made about a different \
             attempt: {update}"
        );
        // Direction, not just presence: migrating new→old passes every
        // assertion above and every policy test, while moving the secrets
        // backwards onto the locator the edit just abandoned. Positions are
        // unwrapped rather than compared as `Option`s, so an argument that
        // stops appearing fails instead of comparing `None < Some(_)`.
        let call = &update[update
            .find("carry_session_secrets(")
            .expect("checked above")..];
        let call = &call[..call.find(");").expect("the call must be closed")];
        let old_at = call.find("&old_locator").expect("the source locator");
        let new_at = call.find("&new_locator").expect("the destination locator");
        assert!(
            old_at < new_at,
            "the carry runs backwards — the secrets move onto the locator the \
             edit abandoned: {call}"
        );
        // And it runs before the keyring migration is spawned, not inside it:
        // session memory is what the next connect reads first, and the next
        // connect can start as soon as this returns.
        let carry_at = update
            .find("carry_session_secrets(")
            .expect("checked above");
        let spawn_at = update
            .find("spawn_forever(")
            .expect("the keyring migration must still be spawned");
        assert!(
            carry_at < spawn_at,
            "the session carry moved into the spawned keyring work, so it lands \
             three keyring round-trips late and a reconnect in that window \
             re-prompts for a secret the app is holding: {update}"
        );
        // The keyring migration is paired the same way, and fails the same
        // silent way: zipping a locator with itself makes `migrate_secret` a
        // no-op on every key, so the stored password stays under the old
        // locator and the edited connection loses it at the next restart —
        // FRE-75's original bug, restored without moving a call.
        let migration = &update[spawn_at..];
        let from = migration
            .find("secret_keys(&old_locator)")
            .expect("the migration must read the old locator's keys");
        let to = migration
            .find("secret_keys(&new_locator)")
            .expect("the migration must write the new locator's keys");
        assert!(
            from < to,
            "the keyring migration runs backwards, or pairs a locator with \
             itself and silently migrates nothing: {migration}"
        );

        let remove = method_body(
            include_str!("connect.rs"),
            "pub fn remove_saved(mut self, locator: &str) {",
        );
        assert!(
            remove.contains("forget_connection_secrets("),
            "a deleted connection keeps its session passphrase and its parked \
             remember choice, so recreating the URL in the same session \
             re-stores what the deletion removed (FRE-162): {remove}"
        );
        // The keyring deletion is what makes the leftovers dangerous rather
        // than merely stale, so it must still happen.
        assert!(
            remove.contains("delete_password_async"),
            "a removed connection no longer drops its stored credentials: {remove}"
        );
    }

    #[test]
    fn the_locator_keyed_keys_are_one_list_for_every_path_that_walks_them() {
        // Migration and deletion walk the same three keys, and a fourth secret
        // added under a locator has to reach both. Naming them in each place
        // is how one path comes to know about `#ssh` and another not to.
        const URL: &str = "postgres://u@h:5432/db";
        assert_eq!(
            secret_keys(URL),
            [
                URL.to_string(),
                format!("{URL}#ssh"),
                format!("{URL}#entra")
            ],
            "the database password must stay under the bare locator — the \
             keyring account every pre-FRE-162 connect already reads"
        );
        let distinct: HashSet<String> = secret_keys(URL).into_iter().collect();
        assert_eq!(
            distinct.len(),
            3,
            "two secrets share a key and will overwrite each other"
        );

        let update = method_body(
            include_str!("connect.rs"),
            "pub fn update_saved(mut self, old_locator: String, connection: SavedConnection) {",
        );
        let remove = method_body(
            include_str!("connect.rs"),
            "pub fn remove_saved(mut self, locator: &str) {",
        );
        for (path, body) in [("update_saved", &update), ("remove_saved", &remove)] {
            assert!(
                body.contains("secret_keys("),
                "{path} names the keyring keys itself again, so the next \
                 secret keyed by locator reaches one path and not the other: \
                 {body}"
            );
        }
    }

    #[test]
    fn a_re_prompt_offers_the_choice_the_user_already_made() {
        // FRE-162. `remember` was `use_signal(|| true)`, so every re-prompt
        // arrived freshly ticked: unticking the box and then mistyping the
        // passphrase silently restored the decision to store it.
        const URL: &str = "postgres://u@h:5432/db";

        // Unticked, typed, rejected: the re-prompt still offers unticked.
        let mut pending = HashMap::new();
        park_ssh_remember(&mut pending, URL, false);
        assert!(
            !withdraw_ssh_remember(&mut pending, URL),
            "a mistyped passphrase re-ticked a box the user had cleared"
        );

        // Ticked, typed, rejected: still ticked, and the intent is withdrawn
        // either way — it was made about the passphrase that just failed.
        let mut pending = HashMap::new();
        park_ssh_remember(&mut pending, URL, true);
        assert!(withdraw_ssh_remember(&mut pending, URL));
        assert!(
            !redeem_ssh_remember(&mut pending, URL, Some(PassphraseSource::Session)),
            "the choice for a rejected passphrase stayed parked, where a later \
             attempt can redeem it"
        );

        // Never asked at all: a passphrase read back from the keyring, or one
        // typed into the connection form — which stashes it in session memory
        // without parking a choice. Both must offer the default, and "no
        // answer" is only distinguishable from "answered no" because the
        // answer itself is stored. While this was a `HashSet` the two were the
        // same state, which silently unticked the form user's re-prompt.
        let mut pending = HashMap::new();
        assert!(
            withdraw_ssh_remember(&mut pending, URL),
            "a first prompt for this connection came up unticked"
        );
        park_ssh_remember(&mut pending, "postgres://u@other:5432/db", false);
        assert!(
            withdraw_ssh_remember(&mut pending, URL),
            "another connection's declined choice decided this prompt"
        );

        // Another connection's parked choice is not withdrawn with it.
        let mut pending = HashMap::new();
        park_ssh_remember(&mut pending, "postgres://u@other:5432/db", true);
        let _ = withdraw_ssh_remember(&mut pending, URL);
        assert_eq!(pending.len(), 1);
    }

    #[test]
    fn only_a_keyring_sourced_passphrase_is_deleted_when_it_is_rejected() {
        const KEY: &str = "postgres://u@h:5432/db#ssh";

        // Typed at the prompt and rejected: the keyring copy is somebody
        // else's — the *saved* passphrase, which this attempt says nothing
        // about. Deleting it here is the bug (FRE-151).
        let mut session = HashMap::from([(KEY.to_string(), "typ0".to_string())]);
        assert!(
            !forget_failed_ssh_passphrase(&mut session, KEY, PassphraseSource::Session),
            "a mistyped passphrase must not delete the stored one"
        );

        // It still leaves session memory: it is wrong, and leaving it would
        // shadow the keyring on the next attempt — the connect would then keep
        // failing on a value the user has already replaced.
        assert!(
            !session.contains_key(KEY),
            "the rejected passphrase is still in session memory, where it will \
             be tried again ahead of the keyring"
        );

        // Read back from the keyring and rejected: only passphrases a tunnel
        // open accepted are ever stored, so this one has gone stale and both
        // copies go.
        let mut session = HashMap::from([(KEY.to_string(), "stale".to_string())]);
        assert!(
            forget_failed_ssh_passphrase(&mut session, KEY, PassphraseSource::Keyring),
            "a stale stored passphrase must be dropped, or every connect \
             re-offers the credential the server just refused"
        );
        assert!(!session.contains_key(KEY));

        // Unrelated entries are never collateral.
        let mut session = HashMap::from([("other#ssh".to_string(), "keep".to_string())]);
        let _ = forget_failed_ssh_passphrase(&mut session, KEY, PassphraseSource::Keyring);
        assert_eq!(session.get("other#ssh").map(String::as_str), Some("keep"));
    }

    #[test]
    fn open_tunnel_decides_on_the_passphrase_s_source_not_its_presence() {
        // The test above proves the policy; this proves `open_tunnel` uses it.
        // The two are separable — the policy would stay green against a caller
        // that never consults it, which is exactly the shape of the original
        // bug: `had_passphrase` was a bool, so a session hit and a keyring hit
        // were indistinguishable and both deleted the stored secret.
        let body = open_tunnel_body();
        assert!(
            !body.contains("forget_stale_secret"),
            "open_tunnel is deleting the keyring entry unconditionally again — \
             that is the FRE-151 bug: `stash_ssh_passphrase` puts the user's \
             *unvalidated* typing in session memory, so one typo destroys the \
             saved passphrase"
        );
        assert!(
            body.contains("forget_failed_ssh_passphrase("),
            "the rejection path must go through the shared policy"
        );
        // Which read produced which label is the whole fix, so that *binding*
        // is what gets asserted — not that both names appear somewhere. Trading
        // the two labels between the arms leaves every name, the call and the
        // gate intact while restoring the bug exactly, so the body is split at
        // the two reads and each half checked for its own label alone.
        //
        // Sliced by the reads themselves rather than by `match`/`if let` shape,
        // so a refactor of the surrounding control flow doesn't quietly stop
        // testing anything.
        let read = &body[..body
            .find("stored.unzip()")
            .expect("value and source are read together")];
        let session_at = read
            .find("session_passwords.read()")
            .expect("session memory is consulted");
        let keyring_at = read
            .find("get_password_async")
            .expect("the keyring is consulted");
        assert!(
            session_at < keyring_at,
            "the keyring is consulted before session memory — the passphrase \
             just typed at the prompt would lose to a stale stored one, and the \
             re-entrant retry would never see what the user entered"
        );
        let (session_read, keyring_read) = (&read[session_at..keyring_at], &read[keyring_at..]);
        assert!(
            session_read.contains("PassphraseSource::Session")
                && !session_read.contains("PassphraseSource::Keyring"),
            "the session read is not labelled Session — with the labels traded, \
             a passphrase typed at the prompt is read as a stale stored secret, \
             which is the FRE-151 bug verbatim"
        );
        assert!(
            keyring_read.contains("PassphraseSource::Keyring")
                && !keyring_read.contains("PassphraseSource::Session"),
            "the keyring read is not labelled Keyring, so a genuinely stale \
             stored passphrase is never cleaned up"
        );
        // And no constant anywhere in the rejection block — not merely inside
        // the call, since a `source.or(Some(…))` default would sit just outside
        // it and delete a correct entry whenever a keyring read errored.
        assert!(
            !method_body(&body, "if let Some(source) = ").contains("PassphraseSource::"),
            "the rejection path is manufacturing a source instead of using the \
             one that was tracked"
        );

        // And the delete must be *inside* the gate, not merely somewhere near
        // it: two independent `contains` checks pass just as well when the
        // delete has been moved out beside an inert `if`.
        let gate = method_body(&body, "if drop_stored {");
        assert!(
            gate.contains("delete_password_async(secret_key)"),
            "the keyring delete must be conditional on the policy's verdict"
        );
        assert_eq!(
            body.matches("delete_password_async").count(),
            1,
            "a second, ungated delete would undo the gate above"
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

    fn v(names: &[&str]) -> Vec<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn a_renamed_group_keeps_its_fold_and_leaves_no_dead_name() {
        let mut collapsed = v(&["Production", "Archive"]);
        assert!(collapsed_needs_rename(&collapsed, "Production"));
        rename_collapsed(&mut collapsed, "Production", "Prod (live)");
        assert_eq!(collapsed, v(&["Prod (live)", "Archive"]));
        assert!(
            !collapsed.iter().any(|n| n == "Production"),
            "the old name must not linger"
        );

        // An expanded group has no fold to carry, and renaming it must not
        // invent one.
        let mut collapsed = v(&["Archive"]);
        assert!(!collapsed_needs_rename(&collapsed, "Production"));
        rename_collapsed(&mut collapsed, "Production", "Prod (live)");
        assert_eq!(collapsed, v(&["Archive"]));

        // Renaming onto a name already in the list leaves it once, not twice.
        let mut collapsed = v(&["A", "B"]);
        rename_collapsed(&mut collapsed, "A", "B");
        assert_eq!(collapsed, v(&["B"]));
    }

    #[test]
    fn a_deleted_groups_fold_is_forgotten() {
        // Left behind, it would re-collapse a group later made with the same
        // name — a fold the user never set, restored from a group that no
        // longer exists.
        let mut collapsed = v(&["Production", "Archive"]);
        forget_collapsed(&mut collapsed, "Production");
        assert_eq!(collapsed, v(&["Archive"]));
        // Deleting an expanded (or unknown) group changes nothing.
        forget_collapsed(&mut collapsed, "Production");
        assert_eq!(collapsed, v(&["Archive"]));
    }
}
