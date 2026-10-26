//! Connection-lifecycle orchestration: the per-engine connect flows
//! (password, SSH tunnel, Microsoft Entra) and the in-flight bookkeeping the
//! connections screen renders.
//!
//! Split out of [`super`] because this is procedural async workflow rather
//! than state definition: it reads and writes [`AppState`]'s signals, but
//! nothing here defines what the app *is*, only how a connection comes to
//! exist. What the flows decide about secrets is in
//! [`secrets_policy`](super::secrets_policy), the per-engine data they take in
//! [`backend`](super::backend), and the saved list they persist into in
//! [`saved`](super::saved).
//!
//! Most of the source-reading tests below read all four files at once
//! (`source_reading::connect_code`), so a function moving between them is not
//! a failure — and a scan over the connect code cannot miss one of them.

use super::*;

use super::backend::*;
use super::saved::*;
use super::secrets_policy::*;

/// A token an Entra sign-in acquired and, when it was redeemed from storage,
/// the sign-out generation it was read at ([`kept_after_sign_in`]): the two
/// travel together from [`AppState::acquire_entra`] to the decision to keep
/// its refresh token.
pub(super) struct EntraSignIn {
    token: azure::AccessToken,
    redeemed_at: Option<u64>,
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
    /// Opens a database named on the command line or handed over by the OS
    /// — one entry point for `hubro <file>`, `hubro <url>`, and a
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
                    // [`Self::connect_server`] answers that with a prompt whose
                    // answer can delete the keyring entry for the locator. A
                    // mistyped `hubro postgres://u:typ0@host/db` would then
                    // put the saved credential of the connection it names one
                    // unticked box away from deletion.
                    //
                    // This path stores nothing until the connect succeeds, and
                    // never forgets anything when it fails. `remember: None`:
                    // a password that happened to be on a command line has not
                    // been through the "remember password" decision, so the
                    // keyring is left as it is either way — not written, and
                    // a different stored password not deleted.
                    //
                    // Only for a password-authenticated connection, though:
                    // this flow saves with `ServerAuth::Password`, which for an
                    // Entra-saved entry would be the same downgrade by another
                    // route. An Entra connection signs in with a token and has
                    // no use for a password anyway, so it connects as saved.
                    Some(password) if matches!(auth, ServerAuth::Password) => {
                        self.connect_server_with_password(
                            backend, url, name, password, None, tunnel,
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
    /// `backend`. With a tunnel configured, the tunnel opens first
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
            // no-ops unless the locator is genuinely open.
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
                    // Decided by the silent attempt below, either way it goes.
                    remember: true,
                    // Likewise: the attempt finds out where its token was.
                    stored: StoredCopy::Unknown,
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
        // read as "the remembered password may have gone stale" and answered
        // with a prompt whose answer can replace or delete the keyring copy.
        // Anything that writes an *unvalidated* password into
        // `session_passwords` makes a typo look like a stale saved credential,
        // so a password the user has just supplied belongs in
        // [`Self::connect_server_with_password`], which remembers it only once
        // it has worked (see [`Self::open_target`]).
        self.set_step(&url, ConnectStep::Credentials);
        let mut session_password = self.session_passwords.read().get(&url).cloned();
        let in_session = session_password.is_some();
        if session_password.is_none() {
            session_password = crate::secrets::get_password_async(url.clone())
                .await
                .ok()
                .flatten();
        }
        // What a prompt raised below can tell the user about a stored copy —
        // from the reads just made, without asking the keyring again.
        let stored = stored_copy(in_session, session_password.is_some());
        self.set_step(&url, ConnectStep::Opening);
        // One value for what is offered and whether it was a remembered
        // password, so the decision below cannot be told about a password the
        // open never sent — which would read the server's refusal of an empty
        // login as a stale password, and delete it.
        let (credential, had_password) = remembered_credential(session_password.as_deref());
        let result = backend
            .open(&connect_url, credential, tls_host.as_deref(), None)
            .await;
        // Whether to prompt, and whether to drop the remembered password from
        // session memory first, is `password_retry`'s decision, on the
        // structured signal the backends set from the server's error code —
        // never on the message. The keyring copy is not touched here: the
        // code also comes back for a right password (an expired Postgres
        // role, a SQL Server contained user naming a missing database).
        if let Err(err) = &result {
            if let Some(retry) = password_retry(err, had_password) {
                self.release_connect(&url);
                if retry == PasswordRetry::ForgetSessionAndPrompt {
                    self.forget_rejected_password(&url, err.to_string());
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
                    stored,
                }));
                return;
            }
        }
        self.finish_connect(url.clone(), name.clone(), result, live_tunnel);
        self.save_server_if_open(backend, &url, &name, tunnel, ServerAuth::Password);
    }

    /// A kept Entra refresh token for `url`, and where it was kept: session
    /// memory first, then the OS keyring. The one place either is read for it,
    /// so a connect and the session-restore gate agree on whether one exists.
    /// A keyring error means "none", exactly as an absent entry does.
    pub(super) async fn cached_entra_refresh(self, url: &str) -> Option<(String, RefreshSource)> {
        let key = entra_secret_key(url);
        // A statement temporary: the read guard is dropped before the await.
        let session = self.session_passwords.read().get(&key).cloned();
        if let Some(token) = session {
            return Some((token, RefreshSource::Session));
        }
        // The keyring copy is the one a running sign-out is deleting.
        if !keyring_sign_in_redeemable(&self.entra_sign_outs.peek(), url) {
            return None;
        }
        crate::secrets::get_password_async(key)
            .await
            .ok()
            .flatten()
            .map(|token| (token, RefreshSource::Keyring))
    }

    /// Acquires one Entra token for `url`'s connection: redeem the kept
    /// refresh token when there is one, otherwise fall through to
    /// `open_browser`. Also returns where the redeemed token was kept, which
    /// [`redeemed_refresh_remembered`] reads, and whether a remembered one is
    /// still in the keyring afterwards ([`entra_stored_copy`]), which the
    /// sign-in card warns about.
    ///
    /// A refresh token Entra refuses for good is forgotten here, wherever it
    /// was kept: it can never work again, and kept it would cost a token round
    /// trip before every later sign-in card — at startup too, since the
    /// restore gate reads it as a silent sign-in.
    async fn acquire_entra(
        self,
        entra: &EntraAuth,
        resource: &str,
        url: &str,
        open_browser: impl FnOnce(&str) -> Result<(), azure::AzureError>,
    ) -> (
        Result<EntraSignIn, azure::AzureError>,
        Option<RefreshSource>,
        StoredCopy,
    ) {
        // Before the token is read: a sign-out from here on revokes it.
        let began = self.entra_sign_out_generation(url);
        let cached = self.cached_entra_refresh(url).await;
        let acquired = azure::acquire_token(
            entra,
            resource,
            cached.as_ref().map(|(token, _)| token.as_str()),
            &azure::Endpoints::default(),
            azure::INTERACTIVE_TIMEOUT,
            open_browser,
        )
        .await;
        if acquired.refresh_rejected {
            self.forget_entra_refresh(url).await;
        }
        let source = cached.map(|(_, source)| source);
        let stored = entra_stored_copy(source, acquired.refresh_rejected);
        let redeemed_at = acquired.redeemed.then_some(began);
        let signed_in = acquired
            .token
            .map(|token| EntraSignIn { token, redeemed_at });
        (signed_in, source, stored)
    }

    /// Drops a connection's kept refresh token from session memory and the
    /// keyring alike, by the path a sign-out takes. Nobody asked for this one,
    /// so a refused deletion is not reported.
    async fn forget_entra_refresh(self, url: &str) {
        let verdict = self.start_entra_sign_out(url);
        let _ = self.finish_entra_sign_out(url, verdict).await;
    }

    /// Starts a sign-out of `url`'s connection ([`begin_entra_sign_out`]):
    /// synchronous, so from the moment it returns no connect redeems the kept
    /// sign-in and none already under way keeps the one it gets back.
    pub(super) fn start_entra_sign_out(mut self, url: &str) -> RefreshKeyring {
        begin_entra_sign_out(
            &mut self.entra_sign_outs.write(),
            &mut self.session_passwords.write(),
            url,
        )
    }

    /// Finishes a sign-out [`Self::start_entra_sign_out`] began: executes its
    /// keyring half and records that it has run.
    pub(super) async fn finish_entra_sign_out(
        mut self,
        url: &str,
        verdict: RefreshKeyring,
    ) -> RefreshKeyringOutcome {
        let outcome = apply_refresh_keyring(url, verdict).await;
        end_entra_sign_out(&mut self.entra_sign_outs.write(), url);
        outcome
    }

    /// The sign-out generation a connect to `url` begins at, read before the
    /// connect reads the token it redeems ([`kept_after_sign_in`]).
    fn entra_sign_out_generation(self, url: &str) -> u64 {
        entra_sign_out_generation(&self.entra_sign_outs.peek(), url)
    }

    /// Keeps a sign-in's refresh token where `remember` says
    /// ([`place_entra_refresh`]). A keyring that refuses the store leaves it
    /// for the session instead, and says so, as a refused password store does.
    ///
    /// `redeemed_at` is when a token redeemed from storage was read
    /// ([`EntraSignIn`]). A sign-out since then means it comes from a sign-in
    /// the user has revoked, so it is kept nowhere — not in the keyring, and
    /// not for the session.
    async fn keep_entra_refresh(
        mut self,
        url: &str,
        refresh: String,
        remember: bool,
        redeemed_at: Option<u64>,
    ) {
        if !kept_after_sign_in(redeemed_at, self.entra_sign_out_generation(url)) {
            return;
        }
        let placed =
            place_entra_refresh(&mut self.session_passwords.write(), url, refresh, remember);
        match apply_refresh_keyring(url, placed).await {
            // Not reported, as before this outcome had a name: an unticked
            // box on a machine whose keyring cannot be reached is most often
            // one with no keyring at all, and saying so after every such
            // sign-in would be noise. A remembered sign-in that did survive
            // shows itself as a silent sign-in, which "Sign out" can remove.
            RefreshKeyringOutcome::Applied | RefreshKeyringOutcome::Unreachable => {}
            RefreshKeyringOutcome::StoreRefused(refresh) => {
                self.session_passwords
                    .write()
                    .insert(entra_secret_key(url), refresh);
                self.connect_error.set(Some(
                    "connected, but the sign-in could not be stored in the system \
                     keyring — it is remembered for this session only"
                        .to_string(),
                ));
            }
            RefreshKeyringOutcome::RemovalRefused => {
                self.connect_error.set(Some(
                    "connected, but an earlier remembered sign-in could not be removed \
                     from the system keyring — it will still sign you in"
                        .to_string(),
                ));
            }
        }
    }

    /// Signs `url`'s connection out of Entra: forgets the refresh token it
    /// kept, in session memory and in the keyring, so its next connect shows
    /// the sign-in card instead of redeeming the token silently.
    ///
    /// The sign-out is recorded, and the session copy goes, before anything is
    /// spawned ([`Self::start_entra_sign_out`]): from then on no connect
    /// redeems the keyring copy while its deletion runs, and a connect that
    /// read the token before the click keeps nothing it gets back
    /// ([`EntraSignOut`]). The keyring half is the same
    /// [`RefreshKeyring::Delete`] a "don't remember" sign-in executes
    /// ([`apply_refresh_keyring`]), spawned because it is I/O — two keyring
    /// round trips, unbounded on a locked keyring — and detached from the
    /// screen so that leaving it cannot cut a deletion short.
    ///
    /// An open tab on the connection keeps working: its pool holds the tokens
    /// it renews with in memory and never writes them back, so the sign-out
    /// takes effect at the next connect. The notice says so.
    pub fn sign_out_entra(mut self, url: String, name: String) {
        let verdict = self.start_entra_sign_out(&url);
        // Whatever was on screen is about something before this click.
        self.connect_notice.set(None);
        self.connect_error.set(None);
        spawn_forever(async move {
            match self.finish_entra_sign_out(&url, verdict).await {
                RefreshKeyringOutcome::RemovalRefused => {
                    self.connect_error.set(Some(format!(
                        "{name} could not be signed out: the remembered sign-in could \
                         not be removed from the system keyring, so it will still \
                         sign you in"
                    )));
                }
                RefreshKeyringOutcome::Unreachable => {
                    self.connect_error.set(Some(format!(
                        "{name}: the system keyring could not be reached, so a sign-in \
                         remembered there, if there is one, was not removed and will \
                         still sign you in once the keyring is available"
                    )));
                }
                // `StoreRefused` cannot come of a `Delete`; treated as done
                // rather than unreachable, because it would mean nothing was
                // left behind to say anything about.
                RefreshKeyringOutcome::Applied | RefreshKeyringOutcome::StoreRefused(_) => {
                    // A sign-in card already up for this connection read the
                    // keyring before the sign-out; its warning about a
                    // remembered sign-in is now about one that is gone.
                    let card_is_this_one = self
                        .entra_prompt
                        .peek()
                        .as_ref()
                        .is_some_and(|prompt| prompt.url == url);
                    if card_is_this_one {
                        if let Some(prompt) = self.entra_prompt.write().as_mut() {
                            prompt.stored = StoredCopy::Absent;
                        }
                    }
                    let open = self.open_locators.peek().iter().any(|(_, l)| *l == url);
                    self.connect_notice.set(Some(sign_out_notice(&name, open)));
                }
            }
        });
    }

    /// The Entra branch of the connect: try to acquire a token silently — a
    /// managed identity always can; interactive only via a cached refresh token
    /// (the browser opener errors, so a missing/expired refresh falls through to
    /// the sign-in card rather than opening a window here). The tunnel is
    /// already open; on a silent success we connect, otherwise park the sign-in.
    async fn connect_server_entra(
        mut self,
        backend: ServerBackend,
        mut pending: EntraPrompt,
        connect_url: String,
        tls_host: Option<String>,
        live_tunnel: Option<Tunnel>,
    ) {
        self.set_step(&pending.url, ConnectStep::SigningIn);
        let (token, source, stored) = self
            .acquire_entra(
                &pending.entra,
                backend.entra_resource,
                &pending.url,
                refuse_browser,
            )
            .await;
        match token {
            Ok(token) => {
                pending.remember = redeemed_refresh_remembered(source);
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
                pending.remember = card_offers_remember(source);
                pending.stored = stored;
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
    /// and on success keeps the refresh token (never the access token) where
    /// `pending.remember` says, and saves the connection with its Entra auth
    /// mode.
    ///
    /// The answer is applied only when the database login succeeds: unticking
    /// the box and then being refused by the database leaves an older
    /// remembered token in the keyring until a sign-in does connect.
    async fn finish_entra_connect(
        mut self,
        backend: ServerBackend,
        pending: EntraPrompt,
        connect_url: &str,
        tls_host: Option<String>,
        signed_in: EntraSignIn,
        live_tunnel: Option<Tunnel>,
    ) {
        let EntraSignIn { token, redeemed_at } = signed_in;
        let EntraPrompt {
            url,
            name,
            tunnel,
            entra,
            remember,
            ..
        } = pending;
        // The access token moves into the login and the pool's renewal, which
        // holds it and the refresh token in memory for the pool's lifetime and
        // writes neither anywhere: only the refresh token below is ever
        // written, and only where `remember` says.
        let renewal = entra_renewal(&entra, backend.entra_resource, &token);
        let result = backend
            .open(
                connect_url,
                ServerCredential::EntraToken(token.secret),
                tls_host.as_deref(),
                Some(renewal),
            )
            .await;
        let connected = result.is_ok();
        self.finish_connect(url.clone(), name.clone(), result, live_tunnel);
        if connected {
            self.entra_prompt.set(None);
            // Kept for silent renewals: in the keyring only on `remember`.
            // Exposed only to be kept where the user's answer says.
            if let Some(refresh) = token.refresh_token.map(|r| r.expose().to_string()) {
                self.keep_entra_refresh(&url, refresh, remember, redeemed_at)
                    .await;
            }
            self.save_server_if_open(backend, &url, &name, tunnel, ServerAuth::Entra(entra));
        }
    }

    /// Resumes an interactive Entra connect from the sign-in card: opens the
    /// browser, waits for the redirect, and connects with the acquired token.
    pub async fn connect_server_with_entra_signin(
        mut self,
        backend: ServerBackend,
        mut prompt: EntraPrompt,
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
        let (token, _, stored) = self
            .acquire_entra(
                &prompt.entra,
                backend.entra_resource,
                &prompt.url,
                open_browser,
            )
            .await;
        match token {
            Ok(token) => {
                // `prompt.remember` is the card's answer, given just now: it
                // decides, whatever an earlier sign-in was told.
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
                // Re-raise the card so the user can retry the sign-in in
                // place. It carries the answer they gave, so a failed sign-in
                // does not re-tick a box they cleared — and what this attempt
                // learned about a remembered sign-in, which it may have just
                // deleted as refused.
                prompt.stored = stored;
                self.entra_prompt.set(Some(prompt));
            }
        }
    }

    /// Completes the password prompt: connects with the entered password
    /// (through the tunnel when one is configured). On success the password
    /// always lives in session memory, and [`stored_password_after_connect`]
    /// decides the keyring: stored when `remember` is ticked (silently staying
    /// session-only when no keyring is available), a different stored one
    /// deleted when it is not, and nothing touched when nobody was asked.
    pub async fn connect_server_with_password(
        mut self,
        backend: ServerBackend,
        url: String,
        name: String,
        password: String,
        remember: Option<bool>,
        tunnel: Option<TunnelConfig>,
    ) {
        self.connect_error.set(None);
        // The prompt replaces the reservation made by connect_server, so
        // re-reserve here.
        if self.focus_or_reserve(&url) {
            // Already open (or reserved): no connect runs, so the save
            // below never fires. An edit still has to land —
            // save_server_if_open no-ops unless the locator is genuinely open.
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
                None,
            )
            .await;
        if result.is_ok() {
            // Only an unticked answer consults what is stored, so only it reads
            // the keyring: a locked wallet can raise an unlock dialog. An
            // unreadable keyring reads as nothing stored, so it never licenses
            // a delete.
            let in_keyring = match remember {
                Some(false) => crate::secrets::get_password_async(url.clone())
                    .await
                    .ok()
                    .flatten(),
                _ => None,
            };
            match stored_password_after_connect(remember, in_keyring.as_deref(), &password) {
                StoredPassword::Store => {
                    // Off-thread; surface a non-fatal notice when the user
                    // asked to remember but the keyring store failed.
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
                StoredPassword::Delete => {
                    let delete = crate::secrets::delete_password_async(url.clone()).await;
                    if delete.is_err() {
                        self.connect_error.set(Some(
                            "connected, but the previous password could not be removed from \
                             the system keyring"
                                .to_string(),
                        ));
                    }
                }
                StoredPassword::Keep => {}
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
    /// accepts the passphrase. That moment is both the condition for storing
    /// it (only an accepted passphrase may reach the keyring) and — since the
    /// connect frequently resumes elsewhere, via a host-key or password
    /// prompt — the only point that reliably happens.
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
        // open has actually accepted this passphrase.
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
        self.set_ssh_remember(&url, Some(remember));
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
    pub fn set_ssh_remember(mut self, url: &str, remember: Option<bool>) {
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

    /// Drops a database password the server just refused from session memory
    /// and surfaces `message`, so the retry prompt is not silently re-offered
    /// the credential that was rejected.
    ///
    /// The keyring copy stays: the refusal code also comes back for a right
    /// password, so only a different password connecting
    /// ([`stored_password_after_connect`]) replaces or deletes it.
    fn forget_rejected_password(mut self, url: &str, message: String) {
        self.session_passwords.write().remove(url);
        self.connect_error.set(Some(message));
    }

    /// Completes the host-key trust prompt: records the offered key in
    /// hubro's known_hosts store, then re-runs the connect (which now finds
    /// the host trusted). A failure to persist surfaces as a connect error.
    pub async fn trust_host_and_connect(mut self, prompt: HostKeyPrompt) {
        self.host_key_prompt.set(None);
        let Some(write_path) = self.paths.peek().known_hosts.clone() else {
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
    /// unavailable. Only [`Self::open_tunnel`] calls it, once a tunnel has
    /// accepted the passphrase and [`redeem_ssh_remember`] has said yes.
    async fn persist_ssh_passphrase(mut self, url: &str) {
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
        // that decision.
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
        let known_hosts = self.paths.peek().known_hosts_read();
        match Tunnel::open(config.clone(), passphrase, target.0, target.1, &known_hosts).await {
            Ok(live) => {
                // The tunnel opened, so the passphrase it was handed is good
                // as far as anything here can tell: the key loaded and the
                // session it authenticated came up. Note what that is *not* —
                // the server never sees this passphrase (it decrypts a local
                // key file), and for an unencrypted key it was ignored
                // outright. It is the condition placed on writing it to the
                // keyring, and the reason the write lives here rather than
                // wherever the connect eventually ends.
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
                // cleared — one call, because reading the answer
                // after withdrawing it would always read the default.
                let remember = withdraw_ssh_remember(&mut self.ssh_remember.write(), url);
                if let Some(source) = source {
                    // Something was tried and rejected. It leaves session
                    // memory either way; only a keyring-sourced one is stale
                    // enough to delete. The write guard is a
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
                    // Not looked up: this prompt's answer decides only the
                    // passphrase, and never deletes a stored one.
                    stored: StoredCopy::Unknown,
                }));
                None
            }
            // First contact parks the connect behind a trust-on-first-use
            // prompt; trusting persists the key and retries. Everything else,
            // a changed (possible MITM) or revoked host key included, is a
            // hard refusal the user resolves out-of-band. Which is which is
            // `into_trust_prompt`'s decision, not this match's.
            Err(err) => match err.into_trust_prompt() {
                Ok(info) => {
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
                Err(err) => {
                    self.fail_connect(url, err.to_string());
                    None
                }
            },
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
        // overwrite the task handle and strand the running connect. What it
        // can still change is where the tab opens: a plain click on a row
        // whose connect runs in the background — a shift-click's, or a
        // session restore's — asks for the tab in front, and gets it.
        if let Some(request) = self.connect_requests.write().get_mut(&key) {
            request.focus |= focus;
            return;
        }
        // "The next connect asks you to sign in" is answered by this one.
        if self.connect_notice.peek().is_some() {
            self.connect_notice.set(None);
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
        self.restoring.write().remove(locator);
    }

    /// Clears a connect's in-flight state. Returns whether the tab it
    /// produced should be focused — false only when a shift-click or a
    /// session restore asked for the background. Connects with no request
    /// (started from a form, or resumed after a password prompt) focus as
    /// before.
    ///
    /// A restored tab's remembered view goes too: a connect that ends
    /// anywhere but [`Self::finish_connect`]'s success — an error, a prompt —
    /// reopens nothing, and the tab a later attempt opens starts fresh, as it
    /// always has.
    fn release_connect(mut self, locator: &str) -> bool {
        self.connecting.write().retain(|c| c.locator != locator);
        self.restoring.write().remove(locator);
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
                // one its Cancel button.
                //
                // Someone else's request is the other way round: a connect
                // with no request of its own — a form, the command line, a
                // file the OS handed over — wants its tab in front, so the
                // connect already running for it brings the tab forward.
                let mine = dioxus::core::Runtime::current().current_task();
                let mut requests = self.connect_requests.write();
                match requests.get_mut(locator) {
                    Some(request) if Some(request.task) == mine => {
                        requests.remove(locator);
                    }
                    // Not ours, so this connect has no request: one under
                    // the same locator would be the one just matched.
                    Some(request) => request.focus = true,
                    None => {}
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
        // Taken before `release_connect`, which discards it.
        let restored = self.restoring.write().remove(&locator);
        let focus = self.release_connect(&locator);
        match result {
            Ok(pool) => {
                // The marking is read here rather than threaded
                // through each connect_* signature: every path — first
                // connect, reconnect, session restore — funnels through this
                // one place, so a new connect path cannot forget it.
                let marking = self.saved_marking(&locator);
                let id = self.registry.write().insert(name, pool, marking.0);
                if let Some(color) = marking.1 {
                    self.connection_colors.write().insert(id, color);
                }
                if let Some(tunnel) = tunnel {
                    self.watch_tunnel(id, &tunnel);
                    self.tunnels.write().insert(id, tunnel);
                }
                self.open_locators.write().push((id, locator));
                if focus {
                    self.active.set(ActiveView::Connection(id));
                }
                if let Some(restored) = restored {
                    self.apply_restored(id, restored);
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

    /// Marks connection `id` as cut off once its tunnel's SSH session ends
    /// ([`Tunnel::session_ended`]), so its tab can say so.
    fn watch_tunnel(self, id: ConnectionId, tunnel: &Tunnel) {
        let message = format!(
            "{}, so this connection can no longer reach the database. Close the tab and \
             connect again.",
            tunnel.ended_message()
        );
        self.mark_lost_when(id, tunnel.session_ended(), message);
    }

    /// Records `message` for connection `id` once `ended` resolves, if the
    /// connection is still open then.
    ///
    /// Closing the tab drops its tunnel, which ends the session too; by then
    /// the connection has left the registry, and that absence is what tells
    /// the two apart. The watch ends with the session either way, so nothing
    /// outlives the tab.
    fn mark_lost_when(
        self,
        id: ConnectionId,
        ended: impl std::future::Future<Output = ()> + 'static,
        message: String,
    ) {
        let mut lost = self.lost_tunnels;
        let registry = self.registry;
        spawn_forever(async move {
            ended.await;
            if registry.peek().get(id).is_some() {
                lost.write().insert(id, message);
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::super::source_reading::*;
    use super::*;

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
            &connect_code(),
            "pub async fn open_target(self, target: OpenTarget)",
        )
    }

    /// The source of [`AppState::open_tunnel`], read for the reason
    /// [`open_target_body`] documents: reaching the branch it is asked about
    /// needs an SSH server, a keyring and a Dioxus runtime, and the bug there
    /// was a routing mistake visible right here.
    fn open_tunnel_body() -> String {
        method_body(&connect_code(), "async fn open_tunnel(")
    }

    #[test]
    fn the_host_key_prompt_is_raised_only_through_into_trust_prompt() {
        // Which tunnel errors may be trusted is `TunnelError::into_trust_prompt`'s
        // decision, executed by `only_a_first_contact_key_can_be_offered_for_trust`.
        // This pins that the UI asks it: one place raises the prompt, inside
        // its Ok arm, and no arm names a host-key variant to bypass it.
        // Comments stripped, since they name the variants.
        let code: String = open_tunnel_body()
            .lines()
            .filter(|line| !line.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(!code.contains("TunnelError::HostKey"), "{code}");
        assert_eq!(code.matches("host_key_prompt.set(").count(), 1, "{code}");
        let ask = code.find(".into_trust_prompt()").expect("the UI must ask");
        let ok = code[ask..].find("Ok(info)").expect("...and prompt on Ok") + ask;
        let raise = code.find("host_key_prompt.set(").unwrap();
        let refuse = code[ok..].find("Err(err)").expect("...and refuse on Err") + ok;
        assert!(ask < ok && ok < raise && raise < refuse, "{code}");
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
            body.contains("backend, url, name, password, None, tunnel,"),
            "the password route must carry the looked-up tunnel too, and no \
             \"remember\" answer: nobody was asked, so the keyring must be \
             neither written nor, for a different stored password, deleted"
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
        // from the keyring) is rejected, it concludes the stored secret may
        // have gone stale and prompts, and a different password connecting
        // through that prompt deletes or replaces the keyring entry. Writing
        // an unvalidated password there therefore makes a typo look like a
        // stale saved credential — `hubro postgres://u:typ0@host/db` costing
        // the saved connection with the same URL its password.
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
            &connect_code(),
            "pub async fn connect_server_with_ssh_passphrase(",
        )
    }

    #[test]
    fn the_remember_choice_is_redeemed_where_the_passphrase_is_accepted() {
        // Trusting a host key used to drop this choice silently. It is made at
        // the prompt, but the keyring write is permitted only once a tunnel
        // open has *accepted* the passphrase. Those are different moments —
        // and, the part that was wrong here, frequently different connects: an
        // untrusted host key, a database password prompt or an Entra sign-in
        // each park the attempt and finish it from somewhere else.
        //
        // So the prompt parks an intent and `open_tunnel` redeems it. Putting
        // the write back at the prompt stores passphrases nothing validated;
        // gating it on the *connection* being open drops the choice again.
        let body = ssh_passphrase_body();
        assert!(
            body.contains("set_ssh_remember(&url, Some(remember))"),
            "the remember choice is dropped, or parked as something other than \
             what the user actually chose: {body}"
        );
        assert!(
            !body.contains("persist_ssh_passphrase"),
            "the prompt writes the passphrase itself, so one the server never \
             accepted can reach the keyring — which `open_tunnel` then reads \
             as previously validated and deletes on rejection: {body}"
        );
        assert!(
            !body.contains("open_locators"),
            "the prompt is gating on the connection being open again. A \
             connect parked on a host key or a password prompt has not opened \
             yet, which is exactly how the choice went missing: \
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
        // it to the re-prompt, so a typo cannot re-tick a cleared box. Both
        // halves are one call, so this pins the call and
        // `a_re_prompt_offers_the_choice_the_user_already_made` executes what
        // it decides.
        let rejection = &body[rejected..];
        // The binding, not merely the call: `!withdraw_ssh_remember(…)`
        // satisfies a search for the call and leaves the `remember,` field
        // below untouched, so the answer inverts one line above everything
        // else this test reads. That is exactly how it got through.
        //
        // Whitespace-collapsed, so wrapping the line reads as the reformat it
        // is rather than as the answer being severed from its policy.
        assert!(
            collapsed(rejection).contains("let remember = withdraw_ssh_remember("),
            "a rejected passphrase leaves its remember choice parked, or the \
             withdrawn answer is negated on the way to the prompt — which \
             re-ticks a box the user cleared and ends in a keyring write of a \
             passphrase they declined: {rejection}"
        );
        // The field as written too: `remember: !remember,` contains
        // `remember,` and inverts the same decision one line lower.
        //
        // Matched a line at a time, not against a literal containing `\n`:
        // that has failed on Windows checkouts before, and
        // `tests/line_endings.rs` records that this file does not depend on
        // the checkout's line endings.
        assert!(
            rejection.lines().any(|line| line.trim() == "remember,"),
            "the withdrawn choice is not reaching the re-prompt as written, so \
             the box is re-ticked after every typo, or offers the opposite of \
             what the user chose: {rejection}"
        );
        // Negation in any *other* form — a second binding, a `!` on the call,
        // or an inversion one hop away in another function — is covered for
        // every verdict at once by
        // `the_remember_answer_is_not_inverted_at_any_hop_of_its_wire`.
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
             the consent this whole mechanism exists to carry decides nothing: \
             {gate}"
        );
        assert_eq!(
            body.matches("persist_ssh_passphrase").count(),
            1,
            "a second, ungated write would store the passphrase whatever the \
             user chose, leaving the gate above inert"
        );
        // Nor anywhere else: the connection form used to call it directly,
        // gated on its own box, beside whatever answer an earlier prompt had
        // left parked — which the tunnel open then redeemed against the form's
        // passphrase. Code only, so prose naming the old call does not count.
        // Every source file, not a list of them: a call added in a file split
        // off later would escape a list.
        for (file, source) in all_sources() {
            let calls = if file == "ui/state/connect.rs" { 2 } else { 0 };
            assert_eq!(
                code_of(&shipped(&source))
                    .matches("persist_ssh_passphrase(")
                    .count(),
                calls,
                "{file} defines or calls persist_ssh_passphrase somewhere besides \
                 its definition and open_tunnel's gate"
            );
        }
    }

    #[test]
    fn the_remember_answer_is_not_inverted_at_any_hop_of_its_wire() {
        // One `!` anywhere on this wire inverts a decision about a secret
        // while moving no call, leaving every gate, label and ordering
        // assertion around it reading correctly.
        //
        // This test names the **wire**, not a function, because scoping is
        // what kept failing. Six rounds of review closed this class one scope
        // at a time — one field, then one arm, then one function — and every
        // next instance sat exactly one hop outside the last fix: the card's
        // submit call after its binding was pinned, `update_saved`'s gate
        // after its call was pinned, `save_server_if_open`'s gate after the
        // argument resting on it was documented. Adding another per-function
        // sweep would invite the seventh.
        //
        // The wire, in order: the card's checkbox — or the connection form's —
        // → its submit → the state layer parks the answer → `open_tunnel`
        // withdraws or redeems it → the prompt carries it back → the card.
        // Plus the two gates the edit paths hang on, which decide whether any
        // of it runs at all, and the database password's own answer, which
        // the same card and form hand to `connect_server_with_password` to
        // decide whether the stored password is written or deleted.
        //
        // What an inversion costs, so the list is not a mystery to whoever
        // trips it:
        //
        // - `redeem` — the passphrase reaches the keyring exactly when the
        //   user declined it, and never when they asked.
        // - `remember` / `remember_choice` / `offered` — the box, the submit
        //   and the re-prompt disagree about the answer the user gave, so a
        //   typo restores a decision they had cleared.
        // - `drop_stored` — a mistyped passphrase deletes the saved one, and
        //   a genuinely stale one is kept forever.
        // - `is_open` — edits apply exactly when the connect *failed*, which
        //   is the premise `carry_session_secrets` rests its whole
        //   don't-migrate-the-answer argument on.
        const NAMES: &[&str] = &[
            "redeem",
            "withdraw",
            "remember",
            "drop_stored",
            "forget",
            "is_open",
            "offered",
        ];
        let source = &connect_code();
        let card = &shipped(include_str!("../connections.rs"));
        let form = method_body(card, "fn submit_server_form(");
        let hops = [
            ("the card", method_body(card, "fn PasswordPromptCard(")),
            ("the form's submit", form.clone()),
            (
                "the form's question",
                method_body(card, "fn asks_remember("),
            ),
            (
                "the prompt's completion",
                method_body(source, "pub async fn connect_server_with_ssh_passphrase("),
            ),
            (
                "the password connect",
                method_body(source, "pub async fn connect_server_with_password("),
            ),
            ("open_tunnel", open_tunnel_body()),
            ("update_saved", method_body(source, "pub fn update_saved(")),
            (
                "save_server_if_open",
                method_body(source, "fn save_server_if_open("),
            ),
        ];
        for (hop, body) in &hops {
            let code = code_of(body);
            assert_eq!(
                negated(&code, NAMES),
                None,
                "a verdict about a secret is negated in {hop}, which inverts \
                 the decision without moving a call: {code}"
            );
        }

        // Negation is not the only way to sever a decision from its policy:
        // `let redeem = true;` passes the sweep, and a gate can be turned
        // round without a `!` at all. Both are pinned — whitespace-collapsed,
        // so rustfmt wrapping a line is not read as a severed decision.
        let code = collapsed(&code_of(&open_tunnel_body()));
        for binding in [
            "let redeem = redeem_ssh_remember(",
            "let remember = withdraw_ssh_remember(",
            "let drop_stored = forget_failed_ssh_passphrase(",
        ] {
            assert!(
                code.contains(binding),
                "`{binding}…` is gone, so a verdict is decided by something \
                 other than its policy: {code}"
            );
        }
        // The form reads its box only when it showed it — `None` otherwise —
        // and hands that one answer to both the passphrase and the password.
        let form = collapsed(&code_of(&form));
        for binding in [
            "let remember_choice = asks_remember(&form.auth_mode.peek()).then(|| *form.remember.peek());",
            "state.set_ssh_remember(&url, remember_choice);",
            "entered_password, remember_choice, tunnel.clone(),",
        ] {
            assert!(
                form.contains(binding),
                "the connection form no longer has `{binding}`, so the answer its \
                 box gives is not the one parked or acted on: {form}"
            );
        }
        assert!(
            collapsed(&code_of(&method_body(source, "pub fn update_saved(")))
                .contains("if new_locator != old_locator {"),
            "the edit's migration gate is inverted or gone — with `==`, a \
             moving edit strands every session secret and migrates no keyring \
             entry, silently, because both calls inside are no-ops when the \
             locator did not move"
        );
        assert!(
            collapsed(&code_of(&method_body(source, "fn save_server_if_open(")))
                .contains("if is_open {"),
            "the save gate is inverted or gone. `carry_session_secrets` \
             justifies not migrating the parked answer by this gate holding — \
             inverted, edits apply exactly when the connect failed, and the \
             argument is stood on its head"
        );
    }

    /// Every function an Entra token travels through after the sign-in — the
    /// whole wire, named in one place: the connect's renewal (`entra_renewal`)
    /// and the open that hands it to the pool, the provider (`db/entra.rs`),
    /// the silent renewal and the token endpoint calls under it, and each
    /// login that asks for a token — SQL Server's per connection, Postgres's
    /// task and its pre-run renewal, and the registry hooks that call it.
    /// It starts after `finish_entra_connect`, whose one keyring write — the
    /// refresh token, where the user's answer says — is the remember wire's
    /// (`the_entra_remember_answer_is_not_inverted_at_any_hop_of_its_wire`).
    ///
    /// None of it may store, delete or read a secret from the keyring or
    /// session memory — including a bare call after a `use` — prompt or open
    /// a browser, log anything, or format a token, refresh token or password
    /// into text. The token's own type ([`crate::db::entra::Secret`]) prints
    /// `<redacted>`, so a `{}` of it gives nothing away; this catches the
    /// exposed value and the raw strings around the edges.
    #[test]
    fn an_entra_tokens_wire_stores_logs_and_prompts_for_nothing() {
        let connect = &connect_code();
        let azure = include_str!("../../azure.rs");
        let postgres = include_str!("../../db/postgres.rs");
        let mssql = include_str!("../../db/sqlserver/pool.rs");
        let registry = include_str!("../../db/registry.rs");
        let provider = shipped(include_str!("../../db/entra.rs"));
        let wire = [
            ("db/entra.rs", provider),
            ("entra_renewal", method_body(connect, "fn entra_renewal(")),
            (
                "ServerBackend::open",
                method_body(connect, "async fn open("),
            ),
            (
                "azure::renew_silently",
                method_body(azure, "pub async fn renew_silently("),
            ),
            (
                "azure::redeem_refresh_token",
                method_body(azure, "async fn redeem_refresh_token("),
            ),
            (
                "azure::post_token",
                method_body(azure, "async fn post_token("),
            ),
            (
                "azure::acquire_managed_identity",
                method_body(azure, "async fn acquire_managed_identity("),
            ),
            (
                "postgres::renew_login",
                method_body(postgres, "pub(super) async fn renew_login(pool"),
            ),
            (
                "PgConn::renew_login",
                method_body(postgres, "pub(super) async fn renew_login(&self)"),
            ),
            (
                "postgres::open_postgres_entra",
                method_body(postgres, "pub async fn open_postgres_entra("),
            ),
            (
                "postgres::open_postgres_entra_every",
                method_body(postgres, "pub async fn open_postgres_entra_every("),
            ),
            (
                "sqlserver login_config",
                method_body(mssql, "async fn login_config("),
            ),
            (
                "sqlserver open_mssql_with",
                method_body(mssql, "pub async fn open_mssql_with("),
            ),
            (
                "DbPool::begin_script_tx",
                method_body(registry, "pub async fn begin_script_tx("),
            ),
            (
                "DbPool::acquire_script_conn",
                method_body(registry, "pub(super) async fn acquire_script_conn("),
            ),
            (
                "DbPool::begin_read_only_run",
                method_body(registry, "pub(super) async fn begin_read_only_run("),
            ),
            (
                "DbPool::open_postgres_entra",
                method_body(registry, "pub async fn open_postgres_entra("),
            ),
            (
                "DbPool::open_postgres_entra_renewing_every",
                method_body(registry, "pub async fn open_postgres_entra_renewing_every("),
            ),
        ];
        const FORBIDDEN: &[&str] = &[
            // Keyring and session memory, qualified or bare.
            "secrets",
            "keyring",
            "get_password",
            "store_password",
            "delete_password",
            "session_passwords",
            "keep_entra_refresh",
            "place_entra_refresh",
            "forget_entra_refresh",
            // A sign-in the user did not start.
            "acquire_interactive",
            "acquire_token",
            "open_browser",
            "webbrowser",
            // Logging of any kind.
            "println!",
            "eprintln!",
            "print!",
            "eprint!",
            "dbg!",
            "log::",
            "tracing::",
        ];
        const FORMATTERS: &[&str] = &[
            "format!(",
            "format_args!(",
            "write!(",
            "writeln!(",
            "panic!(",
        ];
        const TOKEN_NAMES: &[&str] = &["secret", "token", "refresh", "password", "expose"];
        for (what, body) in wire {
            let code = code_of(&body);
            for forbidden in FORBIDDEN {
                assert!(
                    !code.contains(forbidden),
                    "{what} reaches `{forbidden}`: an Entra token's wire must store, \
                     delete, log and prompt for nothing"
                );
            }
            for formatter in FORMATTERS {
                for call in macro_calls(&code, formatter) {
                    let names = formatted_names(&call);
                    for name in TOKEN_NAMES {
                        assert!(
                            !names.to_lowercase().contains(name),
                            "{what} formats `{name}` into text: {formatter}{call})"
                        );
                    }
                }
            }
        }
    }

    /// The scan above, shown to catch what it is for: a token or password
    /// logged or formatted, a bare keyring call, and not a literal's words.
    #[test]
    fn the_token_wire_scan_sees_a_leak() {
        let leaks = [
            "eprintln!(\"{}\", token.expose());",
            "let t = format!(\"{}\", password);",
            "let t = format!(\"{password}\");",
            "store_password_async(key, refresh).await;",
        ];
        for leak in leaks {
            let code = code_of(leak);
            let caught = ["eprintln!", "store_password"]
                .iter()
                .any(|f| code.contains(f))
                || macro_calls(&code, "format!(").iter().any(|call| {
                    let names = formatted_names(call).to_lowercase();
                    names.contains("password") || names.contains("token")
                });
            assert!(caught, "the scan missed: {leak}");
        }
        let literal = "let url = format!(\"{}/oauth2/v2.0/token\", endpoints.login);";
        let names = formatted_names(&macro_calls(literal, "format!(")[0]);
        assert!(
            !names.contains("token"),
            "a literal's word was read as a binding"
        );
    }

    #[test]
    fn the_entra_remember_answer_is_not_inverted_at_any_hop_of_its_wire() {
        // The refresh token is functionally a password, and reaches the
        // keyring on the same kind of answer. The wire, in order: the card's
        // checkbox → its sign-in → `connect_server_with_entra_signin` →
        // `finish_entra_connect` → `keep_entra_refresh` → `place_entra_refresh`
        // → the keyring write or delete. A silent sign-in joins at
        // `connect_server_entra`, whose answer is where the redeemed token was
        // kept; `acquire_entra` and the restore gate read that, and the former
        // forgets a refused token. A `!` anywhere on it inverts a decision
        // about a credential while moving no call — see
        // `the_remember_answer_is_not_inverted_at_any_hop_of_its_wire` for
        // why this names the whole wire rather than one function.
        //
        // What an inversion costs:
        //
        // - `remember` / `offered` — the token is stored exactly when the
        //   user declined, and a remembered sign-in is deleted.
        // - `refresh_rejected` — a working token is forgotten after every
        //   sign-in, and a dead one is paid for before every card.
        // - `has_refresh` — startup pops sign-in cards for connections it
        //   could restore, and tries to restore ones it cannot.
        const NAMES: &[&str] = &[
            "remember",
            "offered",
            "refresh_rejected",
            "redeemed_refresh_remembered",
            "card_offers_remember",
            "has_refresh",
        ];
        let source = &connect_code();
        let card = method_body(
            &shipped(include_str!("../connections.rs")),
            "fn EntraSignInCard(",
        );
        let signin = method_body(source, "pub async fn connect_server_with_entra_signin(");
        let silent = method_body(source, "async fn connect_server_entra(");
        let finish = method_body(source, "async fn finish_entra_connect(");
        let acquire = method_body(source, "async fn acquire_entra(");
        let keep = method_body(source, "async fn keep_entra_refresh(");
        let cached = method_body(source, "async fn cached_entra_refresh(");
        let forget = method_body(source, "async fn forget_entra_refresh(");
        let restore = method_body(
            &shipped(include_str!("session.rs")),
            "async fn connects_silently(",
        );
        let edit = method_body(source, "pub fn update_saved(");
        let hops = [
            ("the card", &card),
            ("the sign-in", &signin),
            ("the silent connect", &silent),
            ("finish_entra_connect", &finish),
            ("acquire_entra", &acquire),
            ("keep_entra_refresh", &keep),
            ("the restore gate", &restore),
            ("update_saved", &edit),
        ];
        for (hop, body) in hops {
            let code = code_of(body);
            assert_eq!(
                negated(&code, NAMES),
                None,
                "a verdict about the refresh token is negated in {hop}: {code}"
            );
        }

        // Bindings as written, since `let remember = true;` passes the sweep.
        // Whitespace-collapsed, so a rustfmt wrap is not read as a severed
        // decision.
        let pinned = [
            (&card, "let offered = prompt.remember;"),
            (&card, "use_signal(move || offered)"),
            (&card, "prompt.remember = *remember.peek();"),
            (&card, "checked: remember(),"),
            (
                &finish,
                "self.keep_entra_refresh(&url, refresh, remember, redeemed_at)",
            ),
            (
                &keep,
                // No closing paren: a wrapped call gains a trailing comma.
                "place_entra_refresh(&mut self.session_passwords.write(), url, refresh, remember",
            ),
            (
                &restore,
                "let has_refresh = self.cached_entra_refresh(&locator).await.is_some();",
            ),
            (&restore, "entra.can_acquire_silently(has_refresh)"),
        ];
        for (body, binding) in pinned {
            assert!(
                collapsed(&code_of(body)).contains(binding),
                "`{binding}` is gone, so the answer is decided by something \
                 other than the user or its policy: {body}"
            );
        }
        // A failed sign-in re-raises the card with the answer it was given;
        // rewriting it on the way re-ticks a box the user cleared.
        assert!(
            !collapsed(&code_of(&signin)).contains("prompt.remember ="),
            "the sign-in rewrites the card's answer: {signin}"
        );
        // The answer travels in the prompt. The silent connect sets it in
        // each arm from its own policy — what to keep on success, what to
        // offer on failure — and `finish_entra_connect` takes it out as
        // written, a line at a time for CRLF checkouts.
        let succeeded = collapsed(&code_of(&method_body(&silent, "Ok(token) => {")));
        assert!(
            succeeded.contains("pending.remember = redeemed_refresh_remembered(source);"),
            "a silent sign-in keeps its token somewhere other than where the \
             user put it: {succeeded}"
        );
        // Sliced by hand: the arm's pattern has braces of its own.
        let parked_from = silent
            .find("EntraAuth::Interactive { .. }) =>")
            .expect("the silent connect still parks interactive sign-ins");
        let parked_to = parked_from
            + silent[parked_from..]
                .find("Err(err) =>")
                .expect("the parking arm is followed by the failure arm");
        let parked = collapsed(&code_of(&silent[parked_from..parked_to]));
        assert!(
            parked.contains("pending.remember = card_offers_remember(source);"),
            "the card parked after a failed silent attempt offers something \
             other than its policy — a ticked box after a \"don't remember\" \
             this run turns a transient error into a keyring write: {parked}"
        );
        assert!(
            finish.lines().any(|line| line.trim() == "remember,"),
            "finish_entra_connect no longer takes the answer out of the prompt"
        );
        // Taken out, then handed on, and nothing in between: a shadowing
        // `let remember = true;` keeps both lines above as written.
        assert_eq!(
            ident_count(&code_of(&finish), "remember"),
            2,
            "finish_entra_connect rebinds the answer between the prompt and \
             the keyring: {finish}"
        );

        // Where a kept token was found *is* the answer, so the labels are as
        // much a decision as any gate: swapped, a remembered token is read as
        // declined and deleted at its next renewal. Session memory, its
        // label, the keyring, its label — in that order.
        let lookup = collapsed(&code_of(&cached));
        let order = [
            "self.session_passwords.read()",
            "return Some((token, RefreshSource::Session));",
            "crate::secrets::get_password_async(key)",
            ".map(|token| (token, RefreshSource::Keyring))",
        ]
        .map(|step| {
            lookup
                .find(step)
                .unwrap_or_else(|| panic!("`{step}` is gone from the lookup: {lookup}"))
        });
        assert!(
            order.windows(2).all(|pair| pair[0] < pair[1]),
            "the lookup labels a token by the wrong home: {lookup}"
        );
        // And forgetting a refused token clears both homes, by the sign-out's
        // path (`signing_out_leaves_no_sign_in_for_the_next_connect_to_redeem`
        // executes it).
        let forgets = collapsed(&code_of(&forget));
        assert!(
            forgets.contains("let verdict = self.start_entra_sign_out(url);")
                && forgets.contains("self.finish_entra_sign_out(url, verdict).await;"),
            "a refused token survives in one of its homes: {forgets}"
        );

        // The verdict reaches the keyring as placed, with nothing in between
        // to rewrite it.
        assert!(
            collapsed(&code_of(&keep)).contains("match apply_refresh_keyring(url, placed).await {"),
            "keep_entra_refresh executes something other than the placed verdict: {keep}"
        );
        // A refused store falls back to the session copy of *this* token.
        let refused = collapsed(&code_of(&method_body(
            &keep,
            "RefreshKeyringOutcome::StoreRefused(refresh) => {",
        )));
        assert!(
            refused.contains(".insert(entra_secret_key(url), refresh);"),
            "a sign-in the keyring refused is not kept for the session: {refused}"
        );

        // Each keyring call sits in the arm of the verdict that asks for it:
        // swapping them stores a declined sign-in and deletes a remembered one
        // while every binding above still reads correctly.
        let apply = method_body(source, "async fn apply_refresh_keyring(");
        let store = method_body(&apply, "RefreshKeyring::Store(refresh) => {");
        assert!(
            store.contains("store_password_async") && !store.contains("delete_password_async"),
            "{store}"
        );
        let delete = method_body(&apply, "RefreshKeyring::Delete => {");
        assert!(
            delete.contains("delete_password_async") && !delete.contains("store_password_async"),
            "{delete}"
        );
        // And the eviction sits inside the rejection's gate.
        let evict = method_body(&acquire, "if acquired.refresh_rejected {");
        assert!(
            evict.contains("self.forget_entra_refresh(url)"),
            "a refused refresh token is not what gets forgotten: {evict}"
        );

        // An edit moving the locator forgets the old token instead of
        // carrying it: the confirming connect already placed its own by the
        // user's answer. Inside the moved-locator gate — with `==` it would
        // delete the token the connect just placed — and with the old locator,
        // before the spawned migration walks only the carried keys.
        let moved = collapsed(&code_of(&method_body(
            &edit,
            "if new_locator != old_locator {",
        )));
        let spawned_at = moved
            .find("spawn_forever(")
            .expect("the keyring migration is still spawned");
        assert!(
            moved[..spawned_at]
                .contains("let forgotten = self.start_entra_sign_out(&old_locator);"),
            "the old locator is not signed out before the spawn, so its session \
             token outlives the edit by three keyring round trips: {moved}"
        );
        assert!(
            moved.contains("self.finish_entra_sign_out(&old_locator, forgotten).await;")
                && moved.contains("carried_secret_keys(&old_locator)")
                && moved.contains("carried_secret_keys(&new_locator)")
                && ident_count(&moved, "secret_keys") == 0,
            "an edit carries the refresh token across, or forgets the wrong \
             one: {moved}"
        );

        // Every route to the token's key, so a new write or read cannot join
        // the wire without being named here. Removal, an edit and a sign-out
        // reach it through `begin_entra_sign_out`, above.
        let shipped_code = code_only(&shipped(source));
        let named: usize = [
            "fn entra_secret_key(",
            "fn secret_keys(",
            "fn place_entra_refresh(",
            "async fn cached_entra_refresh(",
            "async fn forget_entra_refresh(",
            "async fn keep_entra_refresh(",
            "fn begin_entra_sign_out(",
            "async fn apply_refresh_keyring(",
        ]
        .iter()
        .map(|signature| {
            ident_count(
                &code_only(&method_body(source, signature)),
                "entra_secret_key",
            )
        })
        .sum();
        assert_eq!(
            ident_count(&shipped_code, "entra_secret_key"),
            named,
            "the refresh token's key is used outside the functions that \
             carry the remember answer, so something reads or writes it \
             without consulting it"
        );
        // And nowhere outside the connect code: every other source file, so
        // a file split off later cannot hide a use.
        const CONNECT_FILES: [&str; 4] = [
            "ui/state/connect.rs",
            "ui/state/secrets_policy.rs",
            "ui/state/backend.rs",
            "ui/state/saved.rs",
        ];
        for (file, other) in all_sources() {
            if CONNECT_FILES.contains(&file.as_str()) {
                continue;
            }
            assert_eq!(
                ident_count(&code_only(&shipped(&other)), "entra_secret_key"),
                0,
                "src/{file} uses the refresh token's key outside the functions named above"
            );
        }
    }

    #[test]
    fn a_first_prompt_still_offers_to_remember() {
        // Carrying an earlier answer must not quietly change what happens when
        // there is no earlier answer. The SSH re-prompt's default is executed
        // by `a_re_prompt_offers_the_choice_the_user_already_made`; the
        // database-password prompt has no policy behind it at all — nothing
        // parks that choice — so its default is a literal, and a literal can
        // be flipped without moving a call or failing any policy test.
        let body = method_body(&connect_code(), "pub async fn connect_server(");
        assert!(
            body.contains("remember: true"),
            "the password prompt no longer offers to remember by default, so \
             every saved connection quietly starts re-asking: {body}"
        );
    }

    #[test]
    fn connection_management_routes_every_locator_keyed_store_through_the_policy() {
        // The tests above execute what the policy decides; this pins that the
        // two paths still consult it. They are separable, which is how secrets
        // came to be stranded in the first place: `update_saved` and
        // `remove_saved` handled the keyring keys correctly for a year while
        // walking straight past the two in-memory stores keyed by the same
        // locator, and a policy nobody calls stays green forever.
        //
        // `AppState::new` needs a Dioxus runtime, so the methods themselves
        // are out of reach of a unit test — the call is what is checkable
        // here, and the call is what went missing.
        let update = method_body(
            &connect_code(),
            "pub fn update_saved(mut self, old_locator: String, connection: SavedConnection) {",
        );
        assert!(
            update.contains("carry_session_secrets("),
            "an edit migrates the keyring entries but strands the session \
             passphrase and password under the old locator: {update}"
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
        // the very loss re-keying on edit exists to prevent, restored without
        // moving a call.
        // The pairs are built before the spawn (each with what session memory
        // validated under its new key, read after the carry) and the spawned
        // task walks exactly those.
        let moves_at = update
            .find("let moves")
            .expect("the migration's key pairs must still be built");
        assert!(
            carry_at < moves_at && moves_at < spawn_at,
            "the migration's pairs are read before the session carry, or inside \
             the spawn: {update}"
        );
        assert!(
            update[spawn_at..].contains("in moves {"),
            "the spawned migration walks something other than the pairs: {update}"
        );
        let migration = &update[moves_at..];
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
            &connect_code(),
            "pub fn remove_saved(mut self, locator: &str) {",
        );
        assert!(
            remove.contains("forget_connection_secrets("),
            "a deleted connection keeps its session passphrase and its parked \
             remember choice, so recreating the URL in the same session \
             re-stores what the deletion removed: {remove}"
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
             keyring account every earlier version already reads"
        );
        let distinct: HashSet<String> = secret_keys(URL).into_iter().collect();
        assert_eq!(
            distinct.len(),
            3,
            "two secrets share a key and will overwrite each other"
        );
        // An edit carries all of them but the Entra token, whose home is the
        // user's answer and which `update_saved` forgets instead.
        assert_eq!(
            carried_secret_keys(URL).to_vec(),
            secret_keys(URL)
                .into_iter()
                .filter(|key| *key != entra_secret_key(URL))
                .collect::<Vec<_>>()
        );

        let update = method_body(
            &connect_code(),
            "pub fn update_saved(mut self, old_locator: String, connection: SavedConnection) {",
        );
        let remove = method_body(
            &connect_code(),
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
    fn open_tunnel_decides_on_the_passphrase_s_source_not_its_presence() {
        // The test above proves the policy; this proves `open_tunnel` uses it.
        // The two are separable — the policy would stay green against a caller
        // that never consults it, which is exactly the shape of the original
        // bug: `had_passphrase` was a bool, so a session hit and a keyring hit
        // were indistinguishable and both deleted the stored secret.
        let body = open_tunnel_body();
        assert!(
            !body.contains("forget_rejected_password"),
            "open_tunnel is answering a passphrase rejection with the database \
             password's path, which ignores where the passphrase came from"
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
             so one typo deletes the saved passphrase"
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
    fn the_credentials_rejected_signal_survives_every_hop_to_the_keyring() {
        // Each hop between the server's error code and the keyring was a
        // place the decision could be remade on text, or the signal lost. The
        // refusal itself only prompts and forgets session memory; the keyring
        // copy is replaced or deleted once a different password has connected.
        // The engine suites execute the first hops against real servers
        // (`*_only_a_refused_password_is_a_credential_rejection`), as does
        // `real_server_failures_reach_the_decision_intact` below through
        // `ServerBackend::open`, and
        // `a_stored_password_survives_a_refusal_until_a_different_one_connects`
        // runs the whole path against Postgres; the decisions and the credential
        // pairing are executed above. The rest is read as well — pinned as
        // written, not as "mentions" — because the engine test skips without
        // a server.
        let sources = all_sources();
        let file = |rel: &str| {
            let (_, text) = sources
                .iter()
                .find(|(path, _)| path == rel)
                .unwrap_or_else(|| panic!("no src/{rel}"));
            collapsed(&code_only(&shipped(text)))
        };

        // 1. The classifiers: every connect-time error goes through the one
        //    function that reads the code. A second `DbError::Connect(friendly…)`
        //    beside it would be a login failure that can never prompt — or,
        //    written the other way round, one that always deletes.
        for (rel, code_read) in [
            (
                "db/sqlserver/pool.rs",
                "fn connect_error(err: &TdsError) -> DbError { \
                 connect_error_from(err.code(), friendly_connect_error(err)) }",
            ),
            (
                "db/postgres.rs",
                "fn connect_error(err: &sqlx::Error) -> DbError { \
                 let sqlstate = err.as_database_error().and_then(|db| db.code()); \
                 connect_error_from(sqlstate.as_deref(), friendly_connect_error(err)) }",
            ),
        ] {
            let code = file(rel);
            assert!(
                code.contains(code_read),
                "{rel}: the classifier must decide on the code"
            );
            assert_eq!(
                code.matches("friendly_connect_error(").count(),
                2,
                "{rel}: friendly_connect_error is defined once and called only by \
                 connect_error — any other caller builds a connect error the \
                 classifier never saw"
            );
            assert!(
                method_body(&code, "fn connect_error_from(")
                    .contains("DbError::CredentialsRejected(message)"),
                "{rel}: CredentialsRejected is not built by the classifier"
            );
        }

        // 1b. Built nowhere else in the shipped code: those two constructions
        //     are every construction in src/, error.rs included — no `From`
        //     that would build it from a driver error. Match arms and the
        //     declaration are patterns, not constructions, and are fine
        //     anywhere.
        let mut built = Vec::new();
        for (rel, text) in &sources {
            let code = collapsed(&code_only(&shipped(text)));
            // The variant used as a function — `.map_err(DbError::
            // CredentialsRejected)` — builds one without a `(` after the name,
            // where `constructions_of` would not see it. So every use must be
            // followed by `(`, and is then a construction or a pattern.
            let is_ident = |c: char| c.is_alphanumeric() || c == '_';
            for (at, name) in code.match_indices("CredentialsRejected") {
                let before = code[..at].chars().next_back();
                let after = code[at + name.len()..].chars().next();
                if before.is_some_and(is_ident) || after.is_some_and(is_ident) {
                    continue;
                }
                assert_eq!(
                    after,
                    Some('('),
                    "src/{rel} uses CredentialsRejected without `(` — as a function, \
                     e.g. `.map_err(DbError::CredentialsRejected)`, which constructs it \
                     outside the classifiers"
                );
            }
            let n = constructions_of(&code, "CredentialsRejected(");
            if n > 0 {
                built.push((rel.clone(), n));
            }
        }
        built.sort();
        assert_eq!(
            built,
            vec![
                ("db/postgres.rs".to_string(), 1),
                ("db/sqlserver/pool.rs".to_string(), 1)
            ],
            "CredentialsRejected is constructed outside the two code-reading \
             classifiers. A new match arm is not counted here; if this is one the \
             pattern check in `constructions_of` missed, teach it the shape"
        );
        // And the patterns error.rs needs are still there.
        let error_rs = file("db/error.rs");
        for arm in [
            "CredentialsRejected(String),",
            "| DbError::CredentialsRejected(m) => m,",
            "matches!(self, DbError::CredentialsRejected(_))",
            "DbError::Connect(m) | DbError::CredentialsRejected(m) => {",
        ] {
            assert!(error_rs.contains(arm), "error.rs no longer has `{arm}`");
        }

        // 2. The registry passes the error through untouched — each body as
        //    written, so no `map_err`, `or_else` or `match` can remap it.
        let registry = file("db/registry.rs");
        for (open, body) in [
            (
                "pub async fn open_postgres(",
                "pub async fn open_postgres(url: &str) -> Result<DbPool, DbError> { \
                 Ok(DbPool::Postgres(postgres::open_postgres(url).await?)) }",
            ),
            (
                "pub async fn open_mssql(",
                "pub async fn open_mssql(url: &str) -> Result<DbPool, DbError> { \
                 Ok(DbPool::SqlServer(sqlserver::open_mssql(url).await?)) }",
            ),
            (
                "pub async fn open_mssql_with(",
                "pub async fn open_mssql_with( url: &str, auth: &sqlserver::MssqlAuth, \
                 tls_host: Option<&str>, ) -> Result<DbPool, DbError> { \
                 Ok(DbPool::SqlServer( sqlserver::open_mssql_with(url, auth, tls_host).await?, )) }",
            ),
        ] {
            assert_eq!(
                method_body(&registry, open),
                body,
                "{open} changed. If the change is deliberate and still returns the \
                 driver's DbError untouched, update the pin in \
                 the_credentials_rejected_signal_survives_every_hop_to_the_keyring"
            );
        }

        // 3. So does `ServerBackend::open`, as written.
        // The connect code is split across four files; `open` lives in
        // `backend.rs`, `connect_server` in `connect.rs`.
        let source = collapsed(&code_only(&connect_code()));
        assert_eq!(
            method_body(&source, "async fn open("),
            "async fn open( &self, connect_url: &str, credential: ServerCredential<'_>, \
             tls_host: Option<&str>, renewal: Option<Arc<EntraTokens>>, ) \
             -> Result<DbPool, DbError> { \
             let ServerLogin { url, aad_token } = self.login(connect_url, credential)?; \
             match self.kind { BackendKind::SqlServer => { \
             let auth = match (aad_token, renewal) { \
             (Some(_), Some(tokens)) => MssqlAuth::Entra(tokens), \
             (Some(token), None) => MssqlAuth::AadToken(token), \
             (None, _) => MssqlAuth::Password, }; \
             DbPool::open_mssql_with(&url, &auth, tls_host).await } _ => { \
             debug_assert!(aad_token.is_none(), \"a Postgres token belongs in the URL\"); \
             match renewal { Some(tokens) => DbPool::open_postgres_entra(&url, tokens).await, \
             None => DbPool::open_postgres(&url).await, } } } }",
            "ServerBackend::open changed — it may be remapping the error. If the \
             change is deliberate and still returns the driver's DbError untouched, \
             update the pin in \
             the_credentials_rejected_signal_survives_every_hop_to_the_keyring"
        );

        // 4. `connect_server`: the decision's inputs and its consequence, in
        //    order, from the reads to the delete. The password is read under
        //    the connection's own key — session memory, then the keyring —
        //    the credential and the flag come from one call, the open gets
        //    that credential, the verdict gets that flag, and the forget gets
        //    the key the password was read under. Read key and forget key
        //    are both `url`, which nothing in the method rebinds: read under
        //    another key (the SSH passphrase's, say) and a refusal of that
        //    secret would forget the database password.
        let connect = method_body(&source, "pub async fn connect_server(");
        let mut from = 0;
        for step in [
            "let mut session_password = self.session_passwords.read().get(&url).cloned();",
            "if session_password.is_none() {",
            "session_password = crate::secrets::get_password_async(url.clone())",
            "let (credential, had_password) = remembered_credential(session_password.as_deref());",
            ".open(&connect_url, credential, tls_host.as_deref(), None)",
            "if let Err(err) = &result {",
            "if let Some(retry) = password_retry(err, had_password) {",
            "if retry == PasswordRetry::ForgetSessionAndPrompt {",
            "self.forget_rejected_password(&url, err.to_string());",
        ] {
            let at = connect[from..].find(step).unwrap_or_else(|| {
                panic!("connect_server no longer has, after the previous step: `{step}`")
            });
            from += at + step.len();
        }
        assert!(
            !rebinds(&connect, "url"),
            "connect_server rebinds `url`, so the key the password is read under \
             and the key it is forgotten under may no longer be the same"
        );
        for read in ["session_passwords.read()", "get_password_async("] {
            assert_eq!(
                connect.matches(read).count(),
                1,
                "connect_server reads a remembered secret a second way (`{read}`)"
            );
        }
        // Each input bound once and used as written — a rebinding (`let
        // had_password = true`, a second `credential`, a `result.map_err`
        // between the open and the verdict) adds an occurrence.
        for (ident, uses) in [
            ("had_password", 2),
            ("credential", 2),
            ("session_password", 6),
            ("result", 3),
            ("err", 3),
        ] {
            assert_eq!(
                ident_count(&connect, ident),
                uses,
                "`{ident}` is bound or used somewhere new in connect_server. If the \
                 new use is deliberate and does not rebind or remap it, raise the \
                 count in the_credentials_rejected_signal_survives_every_hop_to_the_keyring"
            );
        }
        assert!(
            !connect.contains("ServerCredential::"),
            "connect_server builds a credential of its own instead of the paired one"
        );

        // 5. The verdict reaches the gate unaltered — no `let retry = …`, no
        //    `retry = …` — the forget sits inside the gate, and nothing else in
        //    connect_server forgets or writes session memory. Nothing in it
        //    touches the keyring's copy at all, gate included: the refusal
        //    code also comes back for a right password, so a refusal alone
        //    never costs the stored one.
        let verdict = method_body(&connect, "if let Some(retry) = password_retry(");
        assert_eq!(
            ident_count(&verdict, "retry"),
            2,
            "the verdict is rebound or overridden before the gate. If the new use \
             is deliberate and does not rebind it, raise the count:\n{verdict}"
        );
        let gate = method_body(
            &verdict,
            "if retry == PasswordRetry::ForgetSessionAndPrompt {",
        );
        assert_eq!(
            gate,
            "if retry == PasswordRetry::ForgetSessionAndPrompt { \
             self.forget_rejected_password(&url, err.to_string()); }",
            "the gate holds something other than the one session forget. If the \
             change is deliberate and still forgets only under `url`, update the pin"
        );
        let outside_gate = connect.replacen(&gate, "", 1);
        for forget in ["forget_rejected_password", "session_passwords.write()"] {
            assert!(
                !outside_gate.contains(forget),
                "connect_server reaches `{forget}` outside the gate"
            );
        }
        for keyring in ["delete_password_async", "store_password_async"] {
            assert!(
                !connect.contains(keyring),
                "connect_server writes the keyring (`{keyring}`) — a refused \
                 password is a reason to prompt, not proof the stored one is wrong"
            );
        }
        assert_eq!(
            source.matches("forget_rejected_password(").count(),
            2,
            "forget_rejected_password has a caller besides connect_server's gate"
        );
        assert!(
            verdict.contains("PromptKind::DbPassword"),
            "the password prompt must be raised inside the verdict"
        );
        assert_eq!(
            source.matches("PromptKind::DbPassword").count(),
            1,
            "a second database-password prompt is a second, undecided path"
        );

        // 6. `forget_rejected_password` forgets session memory only, under the
        //    key it was handed — as written, so a keyring delete cannot be
        //    added back inside it.
        assert_eq!(
            method_body(&source, "fn forget_rejected_password("),
            "fn forget_rejected_password(mut self, url: &str, message: String) { \
             self.session_passwords.write().remove(url); \
             self.connect_error.set(Some(message)); }",
            "forget_rejected_password does more than drop the session copy"
        );

        // 6b. Where the stored password *is* replaced or deleted: once the
        //     prompt's password has connected, under the same `url`, decided by
        //     `stored_password_after_connect` on the answer, the stored value
        //     and the password that connected — each as written, in order, and
        //     inside the success gate. The decision itself is executed by
        //     `a_stored_password_survives_until_a_different_one_connects`.
        let with_password = method_body(&source, "pub async fn connect_server_with_password(");
        let success = method_body(&with_password, "if result.is_ok() {");
        let mut from = 0;
        for step in [
            "let in_keyring = match remember { \
             Some(false) => crate::secrets::get_password_async(url.clone()) .await .ok() .flatten(), \
             _ => None, };",
            "match stored_password_after_connect(remember, in_keyring.as_deref(), &password) {",
            "StoredPassword::Store => {",
            "crate::secrets::store_password_async(url.clone(), password.clone()).await;",
            "StoredPassword::Delete => { \
             let delete = crate::secrets::delete_password_async(url.clone()).await;",
            "StoredPassword::Keep => {}",
            "self.session_passwords.write().insert(url.clone(), password);",
        ] {
            let at = success[from..].find(step).unwrap_or_else(|| {
                panic!(
                    "connect_server_with_password's success branch no longer has, after \
                     the previous step: `{step}`\n{success}"
                )
            });
            from += at + step.len();
        }
        for (keyring, count) in [
            ("get_password_async", 1),
            ("store_password_async", 1),
            ("delete_password_async", 1),
        ] {
            assert_eq!(
                with_password.matches(keyring).count(),
                count,
                "connect_server_with_password reaches `{keyring}` outside its decision"
            );
        }
        assert!(
            !rebinds(&with_password, "url") && !rebinds(&with_password, "remember"),
            "connect_server_with_password rebinds `url` or `remember`, so the \
             keyring may be decided on another answer or written under another key"
        );
        // String literals out, so the notices' prose is not counted as code.
        let without_strings = without_string_literals(&with_password);
        for (ident, uses) in [("in_keyring", 2), ("password", 5), ("remember", 3)] {
            assert_eq!(
                ident_count(&without_strings, ident),
                uses,
                "`{ident}` is bound or used somewhere new in \
                 connect_server_with_password. If the new use is deliberate and does \
                 not rebind it, change the count:\n{with_password}"
            );
        }

        // 7. And no code in the UI decides on the words: no string literal in
        //    any shipped file under src/ui names a login or password failure.
        for (rel, text) in sources.iter().filter(|(rel, _)| rel.starts_with("ui/")) {
            let code = code_only(&shipped(text)).to_lowercase();
            for words in ["authentication failed", "login failed"] {
                assert!(
                    !code.contains(words),
                    "src/{rel} names `{words}` in code — a decision on, or a display \
                     of, message text. If it is only displayed, allow-list it here"
                );
            }
        }
    }

    /// Opens through `ServerBackend::open` — the call `connect_server` makes —
    /// with the credential `remembered_credential` pairs, and asks the decision
    /// what follows. `None` is the trust-auth attempt made when nothing is
    /// remembered.
    async fn retry_after(
        backend: ServerBackend,
        url: &str,
        remembered: Option<&str>,
    ) -> Option<PasswordRetry> {
        let (credential, had_password) = remembered_credential(remembered);
        let err = backend
            .open(url, credential, None, None)
            .await
            .err()
            .unwrap_or_else(|| panic!("{url} connected"));
        password_retry(&err, had_password)
    }

    #[tokio::test]
    async fn real_server_failures_reach_the_decision_intact() {
        // Reads the engine variables directly rather than through
        // `tests/common`'s per-binary databases: it creates nothing, and only
        // connects — to the URL's own database with a wrong password, and with
        // the right one to a database that does not exist. Skips, like every
        // engine suite, when a variable is unset.
        let with_db = |url: &str, db: &str| {
            let mut parsed = url::Url::parse(url).unwrap();
            parsed.set_path(&format!("/{db}"));
            parsed.to_string()
        };
        let password_of = |url: &str| {
            let parsed = url::Url::parse(url).unwrap();
            let raw = parsed.password().unwrap_or_default().to_string();
            percent_encoding::percent_decode_str(&raw)
                .decode_utf8()
                .unwrap()
                .into_owned()
        };
        for (var, backend) in [
            ("HUBRO_MSSQL_TEST_URL", ServerBackend::SQL_SERVER),
            ("HUBRO_PG_TEST_URL", ServerBackend::POSTGRES),
        ] {
            let Ok(url) = std::env::var(var) else {
                continue;
            };
            assert_eq!(
                retry_after(backend, &url, Some("definitely-wrong")).await,
                Some(PasswordRetry::ForgetSessionAndPrompt),
                "{var}: a refused password must be forgotten and re-asked"
            );
            let missing = with_db(&url, "hubro_no_such_database");
            assert_eq!(
                retry_after(backend, &missing, Some(&password_of(&url))).await,
                None,
                "{var}: a missing database must keep the password and prompt for nothing"
            );
            // Nothing remembered: the connect tries without a password, and
            // the server's refusal is what raises the prompt at all.
            let mut bare = url::Url::parse(&url).unwrap();
            bare.set_password(None).unwrap();
            assert_eq!(
                retry_after(backend, bare.as_str(), None).await,
                Some(PasswordRetry::Prompt),
                "{var}: a server that wants a password must still be asked for one"
            );
        }
    }

    #[test]
    fn the_sign_in_card_says_a_sign_in_is_remembered_only_while_the_keyring_holds_it() {
        use RefreshSource::{Keyring, Session};
        for (source, rejected, expected, why) in [
            (
                Some(Keyring),
                false,
                StoredCopy::Present,
                "a keyring token that failed only transiently is still there",
            ),
            (
                Some(Keyring),
                true,
                StoredCopy::Absent,
                "a refused keyring token was just deleted by acquire_entra",
            ),
            (
                Some(Session),
                false,
                StoredCopy::Absent,
                "a session-kept token means the keyring holds none",
            ),
            (None, false, StoredCopy::Absent, "no token anywhere"),
        ] {
            assert_eq!(entra_stored_copy(source, rejected), expected, "{why}");
        }

        // The hops from that verdict to the card, which no executed test can
        // reach without a tenant: computed from the attempt's own source and
        // rejection, and handed to the card on both routes that raise it.
        // Comment-stripped and whitespace-collapsed.
        let source = &connect_code();
        for (signature, pin) in [
            (
                "async fn acquire_entra(",
                "let stored = entra_stored_copy(source, acquired.refresh_rejected);",
            ),
            ("async fn acquire_entra(", "(signed_in, source, stored)"),
            (
                "async fn connect_server_entra(",
                "pending.remember = card_offers_remember(source); pending.stored = stored; \
                 self.entra_prompt.set(Some(pending));",
            ),
            (
                "pub async fn connect_server_with_entra_signin(",
                "prompt.stored = stored; self.entra_prompt.set(Some(prompt));",
            ),
        ] {
            let body = collapsed(&code_only(&method_body(source, signature)));
            assert!(body.contains(pin), "{signature}: `{pin}` is gone: {body}");
        }
    }

    #[test]
    fn every_keyring_delete_is_either_warned_about_or_not_the_user_s_answer() {
        // The hints were first derived from the UI side — each box — and a
        // deletion nobody started from (the sign-in card's) went unwarned.
        // This starts from the other end: every place shipped code can delete
        // a keyring entry, each classified. A new one fails here until it is
        // classified, and a user-answered one has to name the hint that
        // warns of it.
        //
        // Limits: it reads only `src/`, stops each file at its test module,
        // and finds deletes by the names of the three `secrets`/`keyring`
        // functions. A delete reached some other way — another crate's
        // keyring call, a macro that builds the name — is invisible. It
        // catches accidents, not evasion.
        const API: &str = "the keyring API itself; its callers are the sites listed here";
        let registry: &[(&str, &str, usize, &str)] = &[
            ("secrets.rs", "delete_password_async", 1, API),
            ("secrets.rs", "delete_in", 1, API),
            (
                "ui/state/connect.rs",
                "connect_server_with_password",
                1,
                "user-answered: `StoredPassword::Delete`, an unticked box once a different \
                 password connects. Warned by `remember_hint` on the password prompt and \
                 `form_remember_hint` on both connection forms",
            ),
            (
                "ui/state/secrets_policy.rs",
                "apply_refresh_keyring",
                1,
                "the executor of a `RefreshKeyring` verdict; every caller is a delete, \
                 classified here under its own name",
            ),
            (
                "ui/state/connect.rs",
                "finish_entra_sign_out",
                1,
                "the keyring half of a sign-out; every caller is a delete, classified \
                 here under its own name",
            ),
            (
                "ui/state/connect.rs",
                "sign_out_entra",
                1,
                "explicit: the user pressed the connection's \"Sign out\"",
            ),
            (
                "ui/state/saved.rs",
                "update_saved",
                1,
                "explicit: an edit moved the locator the sign-in was kept under; the \
                 confirming connect placed its own under the new one",
            ),
            (
                "ui/state/saved.rs",
                "save_or_apply_edit",
                1,
                "explicit: a save that leaves the entry unable to keep an Entra sign-in \
                 (`entra_sign_in_dropped`), which would leave a sign-in no \"Sign out\" \
                 is offered for",
            ),
            (
                "ui/state/connect.rs",
                "keep_entra_refresh",
                1,
                "user-answered: `RefreshKeyring::Delete`, an unticked \"stay signed in\" box \
                 once the sign-in connects. Warned by `entra_remember_hint` on the sign-in \
                 card. The silent path reaches it only for a session-kept token, whose \
                 keyring holds none",
            ),
            (
                "ui/state/secrets_policy.rs",
                "migrate_secret",
                3,
                "explicit: an edit that moved the locator. The old copy is carried to the \
                 new locator, dropped once it is redundant there, or dropped because a \
                 different secret connected. No saved connection's secret is lost: the \
                 old locator belongs to no saved entry after the move, and a different \
                 secret that connected is the current one (ticked, it is stored under the \
                 new locator, so this is a move; unticked, `form_remember_hint` warns)",
            ),
            (
                "ui/state/saved.rs",
                "remove_saved",
                2,
                "explicit: the user removed the connection, which owns these secrets \
                 (its Entra sign-in by the sign-out path)",
            ),
            (
                "ui/state/connect.rs",
                "forget_entra_refresh",
                1,
                "automatic: Entra refused the refresh token for good, so it can never \
                 work again",
            ),
            (
                "ui/state/connect.rs",
                "open_tunnel",
                1,
                "automatic: a keyring-sourced SSH passphrase the tunnel rejected, which is \
                 stale (a session-sourced one is never deleted)",
            ),
        ];
        let mut found: Vec<(String, String)> = Vec::new();
        // `shipped` drops every test module, `pub(crate) mod tests` (secrets.rs
        // exports its own, for the mock keyring) included.
        for (rel, text) in all_sources() {
            let code = code_only(&shipped(&text));
            // Every reference to a delete, not only calls: a fn pointer
            // (`let del = …delete_password_async;`), `.map(delete_password)`
            // and a `use … as` rename all still name the original, so each
            // is counted where it appears — a `use` outside any function as
            // `<module>`, which no entry classifies.
            // `apply_refresh_keyring` executes a delete for its callers, so a
            // call to it is a delete site too: counted only at its definition,
            // every caller would hide behind one entry.
            for ident in [
                "delete_password",
                "delete_password_async",
                "delete_credential",
                "apply_refresh_keyring",
                "finish_entra_sign_out",
            ] {
                for (at, _) in code.match_indices(ident) {
                    let before = &code[..at];
                    let after = &code[at + ident.len()..];
                    let is_ident = |c: char| c.is_alphanumeric() || c == '_';
                    // Part of a longer name (`delete_password` inside
                    // `delete_password_async`, or `x_delete_password`) is that
                    // name's occurrence, not this one; a definition is not a
                    // deletion.
                    if before.chars().next_back().is_some_and(is_ident)
                        || after.chars().next().is_some_and(is_ident)
                        || before.ends_with("fn ")
                    {
                        continue;
                    }
                    let name: String = match before.rfind("fn ") {
                        Some(fn_at) => before[fn_at + 3..]
                            .chars()
                            .take_while(|c| is_ident(*c))
                            .collect(),
                        None => "<module>".to_string(),
                    };
                    found.push((rel.clone(), name));
                }
            }
        }
        for (rel, function) in &found {
            assert!(
                registry.iter().any(|(r, f, ..)| r == rel && f == function),
                "a keyring delete in `{function}` (src/{rel}) is not classified. Add it \
                 to this registry as user-answered (naming the hint that warns of it) \
                 or explicit/automatic (saying why no answer is overridden)"
            );
        }
        for (rel, function, count, why) in registry {
            assert!(!why.is_empty());
            assert_eq!(
                found
                    .iter()
                    .filter(|(r, f)| r == rel && f == function)
                    .count(),
                *count,
                "keyring deletes in `{function}` (src/{rel}) changed; reclassify: {why}"
            );
        }
    }

    /// Runs `connect` on `dom`'s root, driving the dom until it finishes.
    async fn drive(
        dom: &mut dioxus::core::VirtualDom,
        connect: impl std::future::Future<Output = ()> + 'static,
    ) {
        let (finished, mut done) = tokio::sync::oneshot::channel::<()>();
        dom.in_runtime(|| {
            spawn_forever(async move {
                connect.await;
                let _ = finished.send(());
            });
        });
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(30);
        while done.try_recv().is_err() {
            assert!(
                tokio::time::Instant::now() < deadline,
                "the connect never finished"
            );
            let _ = tokio::time::timeout(std::time::Duration::from_millis(50), dom.wait_for_work())
                .await;
            dom.render_immediate(&mut dioxus::core::NoOpMutations);
        }
    }

    /// A change to the saved list lands in the file the state was built on —
    /// the test's temp dir — so a test that saves never writes the user's
    /// connections.toml.
    #[test]
    fn a_test_state_saves_the_list_into_its_own_file() {
        fn empty() -> Element {
            rsx! {}
        }
        let dom = dioxus::core::VirtualDom::new(empty);
        let state = crate::ui::test_dom::provide_state(&dom);
        let file = dom.in_runtime(|| {
            state.create_saved_group("Kept here").unwrap();
            state.paths.peek().connections.clone().unwrap()
        });
        let text = std::fs::read_to_string(&file).unwrap();
        assert!(text.contains("Kept here"), "{text}");
        assert!(
            file.starts_with(std::env::temp_dir()),
            "{} is not a temp file",
            file.display()
        );
    }

    // The keyring guard is held across the awaits on purpose: it keeps the
    // secrets tests from toggling the disable flag mid-flow, and this runtime
    // is single-threaded.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn a_stored_password_survives_a_refusal_until_a_different_one_connects() {
        // The whole path, on a real server and the mock keyring: a role whose
        // `VALID UNTIL` has passed is refused with `28P01` for its *right*
        // password, which used to delete it from the keyring.
        use dioxus::core::VirtualDom;

        use crate::ui::test_dom::provide_state;

        let Ok(admin) = std::env::var("HUBRO_PG_TEST_URL") else {
            return;
        };
        let _keyring = crate::secrets::tests::mock_keyring();
        let mut admin = url::Url::parse(&admin).unwrap();
        admin.set_path("/postgres");
        let mut url = admin.clone();
        url.set_username("hubro_expired_login").unwrap();
        url.set_password(None).unwrap();
        let url = url.to_string();
        let pool = sqlx::PgPool::connect(admin.as_str()).await.unwrap();
        for sql in [
            "DROP ROLE IF EXISTS hubro_expired_login",
            "CREATE ROLE hubro_expired_login LOGIN PASSWORD 'stored' VALID UNTIL '2000-01-01'",
        ] {
            sqlx::query(sql).execute(&pool).await.unwrap();
        }
        let keyring = || crate::secrets::get_password(&url).unwrap();

        fn empty() -> Element {
            rsx! {}
        }
        let mut dom = VirtualDom::new(empty);
        let state = provide_state(&dom);
        dom.rebuild_in_place();
        // `None` connects as a saved connection does; `Some` as the prompt.
        let connect = |password: Option<(&str, bool)>| {
            let (url, password) = (url.clone(), password.map(|(p, r)| (p.to_string(), r)));
            let backend = ServerBackend::POSTGRES;
            let name = "expired".to_string();
            async move {
                match password {
                    None => {
                        state
                            .connect_server(backend, url, name, None, ServerAuth::Password)
                            .await
                    }
                    Some((password, remember)) => {
                        state
                            .connect_server_with_password(
                                backend,
                                url,
                                name,
                                password,
                                Some(remember),
                                None,
                            )
                            .await
                    }
                }
            }
        };
        let open_id = |dom: &VirtualDom| {
            dom.in_runtime(|| {
                state
                    .open_locators
                    .peek()
                    .iter()
                    .find(|(_, l)| *l == url)
                    .map(|(id, _)| *id)
            })
        };

        // The stored password is refused: prompted for, gone from session
        // memory, still in the keyring.
        crate::secrets::store_password(&url, "stored").unwrap();
        drive(&mut dom, connect(None)).await;
        let (prompted, in_session) = dom.in_runtime(|| {
            (
                state.password_prompt.peek().is_some(),
                state.session_passwords.peek().contains_key(&url),
            )
        });
        assert!(prompted, "a refused stored password must raise the prompt");
        assert!(!in_session);
        assert_eq!(
            keyring().as_deref(),
            Some("stored"),
            "the refusal deleted it"
        );

        // The role is renewed. The stored password, retyped unticked, now
        // connects — but it is the same password, so it stays.
        sqlx::query("ALTER ROLE hubro_expired_login PASSWORD 'stored' VALID UNTIL 'infinity'")
            .execute(&pool)
            .await
            .unwrap();
        drive(&mut dom, connect(Some(("stored", false)))).await;
        let id = open_id(&dom).expect("the renewed role must connect");
        assert_eq!(keyring().as_deref(), Some("stored"));
        // The connect saved the connection, into the state's own file: this
        // test once added "expired" to the developer's real list every run.
        let saved = dom.in_runtime(|| state.paths.peek().connections.clone());
        let saved = std::fs::read_to_string(saved.unwrap()).unwrap();
        assert!(saved.contains("hubro_expired_login"), "{saved}");
        dom.in_runtime(|| state.close_connection(id, crate::ui::notice::Gesture::CLICK));

        // A different password, unticked: the stale stored one is deleted.
        sqlx::query("ALTER ROLE hubro_expired_login PASSWORD 'replacement'")
            .execute(&pool)
            .await
            .unwrap();
        drive(&mut dom, connect(Some(("replacement", false)))).await;
        let id = open_id(&dom).expect("the new password must connect");
        assert_eq!(keyring(), None, "a different password connected unticked");
        dom.in_runtime(|| state.close_connection(id, crate::ui::notice::Gesture::CLICK));

        // A different password, ticked: it replaces the stored one.
        crate::secrets::store_password(&url, "stored").unwrap();
        drive(&mut dom, connect(Some(("replacement", true)))).await;
        let id = open_id(&dom).expect("the new password must connect");
        assert_eq!(keyring().as_deref(), Some("replacement"));
        dom.in_runtime(|| state.close_connection(id, crate::ui::notice::Gesture::CLICK));

        crate::secrets::delete_password(&url).unwrap();
        sqlx::query("DROP ROLE hubro_expired_login")
            .execute(&pool)
            .await
            .unwrap();
    }

    /// A Postgres-wire stub on a loopback port that refuses every login with
    /// SQLSTATE `28P01` — enough to raise the password prompt with no server.
    async fn refusing_postgres() -> u16 {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                tokio::spawn(async move {
                    loop {
                        let mut header = [0u8; 8];
                        if socket.read_exact(&mut header).await.is_err() {
                            return;
                        }
                        let len = u32::from_be_bytes(header[..4].try_into().unwrap()) as usize;
                        let code = u32::from_be_bytes(header[4..].try_into().unwrap());
                        // SSLRequest / GSSENCRequest: decline, and the client
                        // carries on in plain text on the same socket.
                        if code == 80877103 || code == 80877104 {
                            let _ = socket.write_all(b"N").await;
                            continue;
                        }
                        let mut rest = vec![0u8; len.saturating_sub(8)];
                        let _ = socket.read_exact(&mut rest).await;
                        let fields: &[u8] =
                            b"SFATAL\0VFATAL\0C28P01\0Mpassword authentication failed\0\0";
                        let mut reply = vec![b'E'];
                        reply.extend_from_slice(&(fields.len() as u32 + 4).to_be_bytes());
                        reply.extend_from_slice(fields);
                        let _ = socket.write_all(&reply).await;
                        return;
                    }
                });
            }
        });
        port
    }

    // See the Postgres test above for why the keyring guard spans the awaits.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn a_refused_connect_tells_its_prompt_what_the_keyring_holds() {
        // Executes the hop from `connect_server`'s reads into the prompt's
        // `stored`, which decides whether the card warns that an unticked box
        // removes a stored password. Inverting what the reads found would
        // warn on a first connect with an empty keyring, and stay silent when
        // the keyring supplied the refused password.
        use dioxus::core::VirtualDom;

        use crate::ui::test_dom::provide_state;

        let _keyring = crate::secrets::tests::mock_keyring();
        let port = refusing_postgres().await;
        fn empty() -> Element {
            rsx! {}
        }
        let mut dom = VirtualDom::new(empty);
        let state = provide_state(&dom);
        dom.rebuild_in_place();

        for (case, keyring, session, expected) in [
            ("keyring", Some("stored"), None, StoredCopy::Present),
            ("nothing", None, None, StoredCopy::Absent),
            (
                "session",
                Some("stored"),
                Some("typed"),
                StoredCopy::Unknown,
            ),
        ] {
            let url = format!("postgres://demo@127.0.0.1:{port}/{case}");
            if let Some(password) = keyring {
                crate::secrets::store_password(&url, password).unwrap();
            }
            if let Some(password) = session {
                let mut passwords = state.session_passwords;
                dom.in_runtime(|| {
                    passwords.write().insert(url.clone(), password.to_string());
                });
            }
            let connect = state.connect_server(
                ServerBackend::POSTGRES,
                url.clone(),
                case.to_string(),
                None,
                ServerAuth::Password,
            );
            drive(&mut dom, connect).await;
            let prompt = dom.in_runtime(|| state.password_prompt.peek().clone());
            let prompt = prompt.unwrap_or_else(|| panic!("{case}: the refusal raised no prompt"));
            assert_eq!(prompt.url, url);
            assert_eq!(prompt.stored, expected, "{case}");
            let mut cleared = state.password_prompt;
            dom.in_runtime(|| cleared.set(None));
            crate::secrets::delete_password(&url).unwrap();
        }
    }

    #[allow(clippy::await_holding_lock)] // see the Postgres test above
    #[tokio::test]
    async fn an_edit_never_carries_a_password_a_different_one_replaced() {
        // An edit that moves the locator migrates the keyring *after* the
        // connect that confirmed it has decided the new locator's entry. Had
        // the migration copied the old password across regardless, a
        // different password connecting unticked would leave the keyring
        // exactly as stale as before — under the new locator.
        use dioxus::core::{NoOpMutations, VirtualDom};

        use crate::ui::test_dom::provide_state;

        let _keyring = crate::secrets::tests::mock_keyring();
        fn empty() -> Element {
            rsx! {}
        }
        let mut dom = VirtualDom::new(empty);
        let state = provide_state(&dom);
        dom.rebuild_in_place();
        // `connected` is what the confirming connect left in session memory
        // under the new locator; `None` when it validated no password.
        let edit = |dom: &mut VirtualDom, old: &str, new: &str, connected: Option<&str>| {
            crate::secrets::store_password(old, "old-password").unwrap();
            let (old, new) = (old.to_string(), new.to_string());
            let connected = connected.map(str::to_string);
            dom.in_runtime(|| {
                if let Some(password) = connected {
                    let mut session = state.session_passwords;
                    session.write().insert(new.clone(), password);
                }
                let connection =
                    ServerBackend::POSTGRES.saved("edited", &new, None, ServerAuth::Password);
                state.update_saved(old, connection);
            });
        };
        // The migration is spawned and always ends by deleting the old copy.
        async fn settle(dom: &mut VirtualDom, old: &str) {
            let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
            while crate::secrets::get_password(old).unwrap().is_some() {
                assert!(
                    tokio::time::Instant::now() < deadline,
                    "the migration never ran"
                );
                let _ =
                    tokio::time::timeout(std::time::Duration::from_millis(20), dom.wait_for_work())
                        .await;
                dom.render_immediate(&mut NoOpMutations);
            }
        }

        // A different password connected (unticked, so nothing was stored
        // under the new locator): the old one is stale and is not carried.
        const OLD: &str = "postgres://u@edit-old:5432/a";
        const NEW: &str = "postgres://u@edit-new:5432/a";
        edit(&mut dom, OLD, NEW, Some("different-password"));
        settle(&mut dom, OLD).await;
        assert_eq!(
            crate::secrets::get_password(NEW).unwrap(),
            None,
            "the edit carried the old password over the one that replaced it"
        );

        // The same password connected — "leave it empty to keep the existing
        // one" — or none was validated: it moves with the edit as before.
        for (from, to, connected) in [
            (
                "postgres://u@same-old:5432/a",
                "postgres://u@same-new:5432/a",
                Some("old-password"),
            ),
            (
                "postgres://u@none-old:5432/a",
                "postgres://u@none-new:5432/a",
                None,
            ),
        ] {
            edit(&mut dom, from, to, connected);
            settle(&mut dom, from).await;
            assert_eq!(
                crate::secrets::get_password(to).unwrap().as_deref(),
                Some("old-password"),
                "{to}: the edit lost a password nothing had replaced"
            );
            crate::secrets::delete_password(to).unwrap();
        }
    }

    #[allow(clippy::await_holding_lock)] // see the Postgres test above
    #[tokio::test]
    async fn a_sign_in_keeps_its_token_in_the_home_its_answer_names() {
        // The keyring half of `place_entra_refresh`'s verdict, executed: an
        // older remembered token is already in the keyring, and the new
        // sign-in's answer decides whether it is replaced or removed. Both
        // must leave the fresh token in exactly the home the answer names.
        use dioxus::core::{NoOpMutations, VirtualDom};

        use crate::ui::test_dom::provide_state;

        let _keyring = crate::secrets::tests::mock_keyring();
        fn empty() -> Element {
            rsx! {}
        }
        let mut dom = VirtualDom::new(empty);
        let state = provide_state(&dom);
        dom.rebuild_in_place();

        for remember in [false, true] {
            let url = format!("postgres://u@keep-{remember}:5432/a");
            let key = entra_secret_key(&url);
            crate::secrets::store_password(&key, "older").unwrap();
            let (task_url, task_key) = (url.clone(), key.clone());
            dom.in_runtime(|| {
                spawn_forever(async move {
                    state
                        .keep_entra_refresh(&task_url, "fresh".to_string(), remember, None)
                        .await;
                    // Marks the task done, whatever it decided.
                    crate::secrets::store_password(&format!("{task_key}-done"), "1").unwrap();
                });
            });
            let done = format!("{key}-done");
            let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
            while crate::secrets::get_password(&done).unwrap().is_none() {
                assert!(tokio::time::Instant::now() < deadline, "never ran");
                let _ =
                    tokio::time::timeout(std::time::Duration::from_millis(20), dom.wait_for_work())
                        .await;
                dom.render_immediate(&mut NoOpMutations);
            }

            let session = dom.in_runtime(|| state.session_passwords.peek().clone());
            let homes = [
                session.get(&key).cloned(),
                crate::secrets::get_password(&key).unwrap(),
            ];
            let expected = if remember {
                [None, Some("fresh".to_string())]
            } else {
                [Some("fresh".to_string()), None]
            };
            assert_eq!(
                homes, expected,
                "remember = {remember}: the token is not in the one home the \
                 answer names — unticked, an older remembered token outliving \
                 the answer keeps the user signed in regardless"
            );
            crate::secrets::delete_password(&key).unwrap();
            crate::secrets::delete_password(&done).unwrap();
        }
    }

    /// Spins `dom` until `done` holds, failing after ten seconds with `what`.
    async fn settle_until(
        dom: &mut dioxus::core::VirtualDom,
        what: &str,
        mut done: impl FnMut(&mut dioxus::core::VirtualDom) -> bool,
    ) {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
        while !done(dom) {
            assert!(tokio::time::Instant::now() < deadline, "{what}");
            let _ = tokio::time::timeout(std::time::Duration::from_millis(20), dom.wait_for_work())
                .await;
            dom.render_immediate(&mut dioxus::core::NoOpMutations);
        }
    }

    /// Runs `cached_entra_refresh(url)` to completion — the read every
    /// connect, sign-in card and restore gate decides on.
    async fn next_connect_finds(
        dom: &mut dioxus::core::VirtualDom,
        state: AppState,
        url: &'static str,
    ) -> Option<(String, RefreshSource)> {
        let found = std::sync::Arc::new(std::sync::Mutex::new(None));
        let slot = found.clone();
        dom.in_runtime(|| {
            spawn_forever(async move {
                let result = state.cached_entra_refresh(url).await;
                *slot.lock().unwrap() = Some(result);
            });
        });
        settle_until(dom, "the lookup never ran", |_| {
            found.lock().unwrap().is_some()
        })
        .await;
        let result = found.lock().unwrap().take().unwrap();
        result
    }

    #[allow(clippy::await_holding_lock)] // see the Postgres test above
    #[tokio::test]
    async fn signing_out_leaves_no_sign_in_for_the_next_connect_to_redeem() {
        // The whole wire of "Sign out", one hop at a time: the row offers it
        // behind `keeps_entra_sign_in` and hands it the row's own locator →
        // `sign_out_entra` → `start_entra_sign_out` (the record, and session
        // memory, synchronously) → `finish_entra_sign_out` →
        // `apply_refresh_keyring` with `Delete` (the keyring). Then both ways
        // a sign-in can come back without a card: the next connect's
        // `cached_entra_refresh`, which must find nothing — even before the
        // deletion has run — and a connect already under way, whose
        // `keep_entra_refresh` must keep nothing it got back. Executed from
        // the method down; the row's two hops are read from its source.
        use dioxus::core::VirtualDom;

        use crate::ui::test_dom::provide_state;

        // The row: the button sits inside the gate and signs out the row it
        // is on. Comment-stripped and whitespace-collapsed.
        let row = collapsed(&code_only(&method_body(
            &shipped(include_str!("../connections.rs")),
            "fn SavedConnectionRow(",
        )));
        let gate = "if row.auth.keeps_entra_sign_in() {";
        let gated = row
            .find(gate)
            .unwrap_or_else(|| panic!("the row no longer gates its sign-out: {row}"));
        let button = method_body(&row[gated..], gate);
        assert!(
            button.contains(
                "let (locator, name) = (row.locator.clone(), row.name.clone()); \
                 move |_| state.sign_out_entra(locator.clone(), name.clone())"
            ),
            "the sign-out button signs out something other than its row: {button}"
        );
        assert_eq!(
            ident_count(&row, "sign_out_entra"),
            1,
            "a sign-out is offered outside the gate: {row}"
        );
        // The acquire reads the generation before the token, and marks only
        // a token redeemed from storage as revocable by a later sign-out; the
        // keep decides on what it was handed.
        let source = &connect_code();
        let acquire = collapsed(&code_of(&method_body(source, "async fn acquire_entra(")));
        let read = acquire
            .find("let began = self.entra_sign_out_generation(url);")
            .unwrap_or_else(|| panic!("the acquire no longer records when it began: {acquire}"));
        let cached = acquire
            .find("self.cached_entra_refresh(url)")
            .expect("the acquire reads the kept token");
        assert!(
            read < cached,
            "the acquire records its start after the read: {acquire}"
        );
        assert!(
            acquire.contains("let redeemed_at = acquired.redeemed.then_some(began);")
                && acquire.contains(".map(|token| EntraSignIn { token, redeemed_at });"),
            "the acquire marks something other than a redeemed token as revocable: {acquire}"
        );
        assert!(
            collapsed(&code_of(&method_body(
                source,
                "async fn finish_entra_connect("
            )))
            .contains("let EntraSignIn { token, redeemed_at } = signed_in;"),
            "finish_entra_connect keeps a token without knowing whether it was redeemed"
        );

        let _keyring = crate::secrets::tests::mock_keyring();
        fn empty() -> Element {
            rsx! {}
        }
        let mut dom = VirtualDom::new(empty);
        let state = provide_state(&dom);
        dom.rebuild_in_place();

        // Two connections, each with a token in both homes (the one-place
        // rule's accepted exception — and the hardest case: either copy
        // alone would sign the user in). Only the first signs out. A tab on
        // it is open, and the card on screen belongs to the other one.
        const URL: &str = "postgres://u@sign-out:5432/a";
        const OTHER: &str = "postgres://u@stays-signed-in:5432/a";
        let key = entra_secret_key(URL);
        let other_key = entra_secret_key(OTHER);
        let card = |url: &str| EntraPrompt {
            url: url.to_string(),
            name: "card".to_string(),
            tunnel: None,
            entra: EntraAuth::interactive_default(),
            backend: BackendKind::Postgres,
            remember: true,
            stored: StoredCopy::Present,
        };
        for k in [&key, &other_key] {
            crate::secrets::store_password(k, "kept").unwrap();
        }
        let began = dom.in_runtime(|| {
            let mut session = state.session_passwords;
            for k in [&key, &other_key] {
                session.write().insert(k.clone(), "kept".to_string());
            }
            let pool =
                DbPool::Sqlite(sqlx::sqlite::SqlitePool::connect_lazy("sqlite::memory:").unwrap());
            let mut registry = state.registry;
            let id = registry.write().insert("prod", pool, WriteProtection::Open);
            let mut open = state.open_locators;
            open.write().push((id, URL.to_string()));
            let mut prompt = state.entra_prompt;
            prompt.set(Some(card(OTHER)));
            let mut error = state.connect_error;
            error.set(Some("an older failure".to_string()));
            // A connect already under way: it began before the click.
            let began = state.entra_sign_out_generation(URL);
            state.sign_out_entra(URL.to_string(), "prod".to_string());
            began
        });
        // Synchronously, before the deletion has run — the keyring still
        // holds the token here: neither copy is redeemable.
        assert_eq!(
            dom.in_runtime(|| state.session_passwords.peek().get(&key).cloned()),
            None,
            "the session copy outlives the click"
        );
        assert_eq!(
            dom.in_runtime(|| state.connect_error.peek().clone()),
            None,
            "an older failure is left standing beside the sign-out"
        );
        assert!(
            !dom.in_runtime(|| keyring_sign_in_redeemable(&state.entra_sign_outs.peek(), URL)),
            "the keyring copy is redeemable while its deletion runs"
        );

        settle_until(&mut dom, "never signed out", |dom| {
            dom.in_runtime(|| {
                state.connect_notice.peek().is_some() || state.connect_error.peek().is_some()
            })
        })
        .await;
        assert_eq!(dom.in_runtime(|| state.connect_error.peek().clone()), None);
        assert_eq!(
            dom.in_runtime(|| state.connect_notice.peek().clone())
                .as_deref(),
            Some(sign_out_notice("prod", true).as_str()),
            "the notice does not say the open tab stays connected"
        );
        assert_eq!(
            [
                dom.in_runtime(|| state.session_passwords.peek().get(&key).cloned()),
                crate::secrets::get_password(&key).unwrap(),
            ],
            [None, None],
            "a signed-out sign-in survives in one of its homes"
        );
        assert_eq!(
            next_connect_finds(&mut dom, state, URL).await,
            None,
            "the next connect still signs in silently"
        );
        assert_eq!(
            dom.in_runtime(|| state.entra_prompt.peek().as_ref().map(|p| p.stored)),
            Some(StoredCopy::Present),
            "another connection's card lost its warning about its own sign-in"
        );

        // The connect that was under way finishes now, with a rotated token
        // and the "remembered" answer its redeemed keyring copy implied: it
        // keeps nothing, anywhere.
        let finished = std::sync::Arc::new(std::sync::Mutex::new(false));
        let flag = finished.clone();
        dom.in_runtime(|| {
            spawn_forever(async move {
                state
                    .keep_entra_refresh(URL, "rotated".to_string(), true, Some(began))
                    .await;
                *flag.lock().unwrap() = true;
            });
        });
        settle_until(&mut dom, "the late keep never ran", |_| {
            *finished.lock().unwrap()
        })
        .await;
        assert_eq!(
            [
                dom.in_runtime(|| state.session_passwords.peek().get(&key).cloned()),
                crate::secrets::get_password(&key).unwrap(),
            ],
            [None, None],
            "a connect that began before the sign-out undid it"
        );
        assert_eq!(next_connect_finds(&mut dom, state, URL).await, None);

        // The lookup's own gate, held open: a sign-out whose deletion has not
        // run yet, against a keyring still holding the token. Driven on the
        // record directly, so the deletion cannot win the race and make the
        // keyring empty for the wrong reason.
        const PENDING: &str = "postgres://u@deletion-pending:5432/a";
        crate::secrets::store_password(&entra_secret_key(PENDING), "kept").unwrap();
        let _ = dom.in_runtime(|| state.start_entra_sign_out(PENDING));
        assert_eq!(
            next_connect_finds(&mut dom, state, PENDING).await,
            None,
            "a connect redeems the keyring copy a sign-out is deleting"
        );
        dom.in_runtime(|| {
            let mut sign_outs = state.entra_sign_outs;
            end_entra_sign_out(&mut sign_outs.write(), PENDING);
        });
        assert_eq!(
            next_connect_finds(&mut dom, state, PENDING).await,
            Some(("kept".to_string(), RefreshSource::Keyring)),
            "once no deletion runs, a token still there (one the keyring refused \
             to delete) is what it is"
        );
        crate::secrets::delete_password(&entra_secret_key(PENDING)).unwrap();

        // Signing one connection out signs no other one out.
        assert_eq!(
            [
                dom.in_runtime(|| state.session_passwords.peek().get(&other_key).cloned()),
                crate::secrets::get_password(&other_key).unwrap(),
            ],
            [Some("kept".to_string()), Some("kept".to_string())],
            "another connection was signed out too"
        );

        // And the card that *is* this connection's is told its remembered
        // sign-in is gone.
        dom.in_runtime(|| {
            let mut prompt = state.entra_prompt;
            prompt.set(Some(card(URL)));
            let mut notice = state.connect_notice;
            notice.set(None);
            state.sign_out_entra(URL.to_string(), "prod".to_string());
        });
        settle_until(&mut dom, "never signed out again", |dom| {
            dom.in_runtime(|| state.connect_notice.peek().is_some())
        })
        .await;
        assert_eq!(
            dom.in_runtime(|| state.entra_prompt.peek().as_ref().map(|p| p.stored)),
            Some(StoredCopy::Absent),
            "the card still warns about the remembered sign-in just removed"
        );
        crate::secrets::delete_password(&other_key).unwrap();
    }

    #[allow(clippy::await_holding_lock)] // see the Postgres test above
    #[tokio::test]
    async fn a_browser_sign_in_after_a_refused_token_is_kept_where_its_card_says() {
        // The stale-token recovery path, executed hop by hop as
        // `acquire_entra` runs it: the generation is read, the kept token is
        // read, Entra refuses it for good so it is forgotten — through the
        // sign-out path, which moves the generation — and the browser sign-in
        // that follows succeeds with "Stay signed in" ticked. That sign-in is
        // fresh consent: its token must reach the keyring, although a
        // sign-out (the connect's own) came after the generation was read.
        use dioxus::core::VirtualDom;

        use crate::ui::test_dom::provide_state;

        let _keyring = crate::secrets::tests::mock_keyring();
        fn empty() -> Element {
            rsx! {}
        }
        let mut dom = VirtualDom::new(empty);
        let state = provide_state(&dom);
        dom.rebuild_in_place();

        const URL: &str = "postgres://u@stale-token:5432/a";
        let key = entra_secret_key(URL);
        crate::secrets::store_password(&key, "stale").unwrap();
        let began = dom.in_runtime(|| state.entra_sign_out_generation(URL));
        assert_eq!(
            next_connect_finds(&mut dom, state, URL).await,
            Some(("stale".to_string(), RefreshSource::Keyring))
        );
        let done = std::sync::Arc::new(std::sync::Mutex::new(false));
        let flag = done.clone();
        dom.in_runtime(|| {
            spawn_forever(async move {
                // `if acquired.refresh_rejected { self.forget_entra_refresh(url) }`
                state.forget_entra_refresh(URL).await;
                // An interactive token: `acquired.redeemed` is false.
                let redeemed_at = false.then_some(began);
                state
                    .keep_entra_refresh(URL, "fresh".to_string(), true, redeemed_at)
                    .await;
                *flag.lock().unwrap() = true;
            });
        });
        settle_until(&mut dom, "the recovery never ran", |_| {
            *done.lock().unwrap()
        })
        .await;
        assert_ne!(
            dom.in_runtime(|| state.entra_sign_out_generation(URL)),
            began,
            "the forgetting no longer counts as a sign-out, so this test no \
             longer covers the case it was written for"
        );
        assert_eq!(
            crate::secrets::get_password(&key).unwrap().as_deref(),
            Some("fresh"),
            "a ticked \"Stay signed in\" was thrown away after a refused token"
        );
        crate::secrets::delete_password(&key).unwrap();
    }

    #[allow(clippy::await_holding_lock)] // see the Postgres test above
    #[tokio::test]
    async fn a_save_that_leaves_no_way_to_keep_a_sign_in_signs_the_connection_out() {
        // Both branches of `save_or_apply_edit` can rewrite a saved entry's
        // auth: the edit, and `add` on a locator already saved (a connect
        // through a different form or auth). Either one that takes away
        // interactive sign-in signs the connection out; either one that keeps
        // it, or never had it, leaves the token alone and starts no sign-out.
        use dioxus::core::VirtualDom;

        use crate::ui::test_dom::provide_state;

        let _keyring = crate::secrets::tests::mock_keyring();
        fn empty() -> Element {
            rsx! {}
        }
        let mut dom = VirtualDom::new(empty);
        let state = provide_state(&dom);
        dom.rebuild_in_place();

        let interactive = ServerAuth::Entra(EntraAuth::interactive_default());
        let managed = ServerAuth::Entra(EntraAuth::ManagedIdentity { client_id: None });
        // (saved auth, saved auth after, through an edit?, signs out)
        for (case, (before, after, edit, signs_out)) in [
            (interactive.clone(), ServerAuth::Password, true, true),
            (interactive.clone(), managed.clone(), false, true),
            (interactive.clone(), ServerAuth::Password, false, true),
            (interactive.clone(), interactive.clone(), false, false),
            (interactive.clone(), interactive.clone(), true, false),
            (ServerAuth::Password, ServerAuth::Password, false, false),
        ]
        .into_iter()
        .enumerate()
        {
            let url = format!("postgres://u@save-{case}:5432/a");
            let key = entra_secret_key(&url);
            crate::secrets::store_password(&key, "kept").unwrap();
            dom.in_runtime(|| {
                let mut saved = state.saved;
                saved
                    .write()
                    .add(ServerBackend::POSTGRES.saved("c", &url, None, before.clone()));
                let mut session = state.session_passwords;
                session.write().insert(key.clone(), "kept".to_string());
                if edit {
                    state.set_pending_edit(url.clone(), url.clone());
                }
                state.save_or_apply_edit(ServerBackend::POSTGRES.saved(
                    "c",
                    &url,
                    None,
                    after.clone(),
                ));
            });
            let what = format!("case {case}: {before:?} → {after:?}, edit {edit}");
            if signs_out {
                assert_eq!(
                    dom.in_runtime(|| state.session_passwords.peek().get(&key).cloned()),
                    None,
                    "{what}: the session copy outlives the save"
                );
                settle_until(
                    &mut dom,
                    &format!("{what}: the keyring copy outlives the save"),
                    |_| crate::secrets::get_password(&key).unwrap().is_none(),
                )
                .await;
            } else {
                // Decided synchronously, so no sign-out started is final.
                assert_eq!(
                    dom.in_runtime(|| entra_sign_out_generation(
                        &state.entra_sign_outs.peek(),
                        &url
                    )),
                    0,
                    "{what}: a save that keeps the sign-in signed out"
                );
                assert_eq!(
                    dom.in_runtime(|| state.session_passwords.peek().get(&key).cloned()),
                    Some("kept".to_string()),
                    "{what}"
                );
                crate::secrets::delete_password(&key).unwrap();
            }
        }
    }

    #[allow(clippy::await_holding_lock)] // see the Postgres test above
    #[tokio::test]
    async fn an_edit_leaves_the_refresh_token_where_the_confirming_connect_put_it() {
        // Where a refresh token is kept is the user's "remember" answer, and
        // the connect confirming an edit has already placed its own token by
        // the answer given then. Whatever the old locator held, in whichever
        // home, must not come along: the token ends up in exactly one home.
        use dioxus::core::{NoOpMutations, VirtualDom};

        use crate::ui::test_dom::provide_state;

        let _keyring = crate::secrets::tests::mock_keyring();
        fn empty() -> Element {
            rsx! {}
        }
        let mut dom = VirtualDom::new(empty);
        let state = provide_state(&dom);
        dom.rebuild_in_place();

        // (old token's home, new answer)
        for (case, (old_in_keyring, remember)) in
            [(true, false), (false, true)].into_iter().enumerate()
        {
            let old = format!("postgres://u@entra-old-{case}:5432/a");
            let new = format!("postgres://u@entra-new-{case}:5432/a");
            let (old_key, new_key) = (entra_secret_key(&old), entra_secret_key(&new));
            // A password too, so the spawned migration has something whose
            // move says it has run.
            crate::secrets::store_password(&old, "pw").unwrap();
            if old_in_keyring {
                crate::secrets::store_password(&old_key, "old-token").unwrap();
            }
            if remember {
                crate::secrets::store_password(&new_key, "new-token").unwrap();
            }
            dom.in_runtime(|| {
                let mut session = state.session_passwords;
                if !old_in_keyring {
                    session.write().insert(old_key.clone(), "old-token".into());
                }
                if !remember {
                    session.write().insert(new_key.clone(), "new-token".into());
                }
                let connection = ServerBackend::POSTGRES.saved(
                    "edited",
                    &new,
                    None,
                    ServerAuth::Entra(EntraAuth::interactive_default()),
                );
                state.update_saved(old.clone(), connection);
            });
            let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
            while crate::secrets::get_password(&old).unwrap().is_some() {
                assert!(tokio::time::Instant::now() < deadline, "never migrated");
                let _ =
                    tokio::time::timeout(std::time::Duration::from_millis(20), dom.wait_for_work())
                        .await;
                dom.render_immediate(&mut NoOpMutations);
            }

            let session = dom.in_runtime(|| state.session_passwords.peek().clone());
            let homes = [
                session.get(&new_key).cloned(),
                crate::secrets::get_password(&new_key).unwrap(),
            ];
            let expected = if remember {
                [None, Some("new-token".to_string())]
            } else {
                [Some("new-token".to_string()), None]
            };
            assert_eq!(
                homes, expected,
                "case {case}: the edit put the old token next to the new one, \
                 so the answer the user just gave is not the one kept"
            );
            assert_eq!(session.get(&old_key), None, "case {case}");
            assert_eq!(
                crate::secrets::get_password(&old_key).unwrap(),
                None,
                "case {case}: the old locator's token outlived the edit"
            );
            crate::secrets::delete_password(&new).unwrap();
            crate::secrets::delete_password(&new_key).unwrap();
        }
    }

    #[tokio::test]
    async fn a_tab_is_flagged_when_its_session_ends_but_not_once_it_has_closed() {
        use dioxus::core::{NoOpMutations, VirtualDom};

        use crate::ui::test_dom::provide_state;

        fn empty() -> Element {
            rsx! {}
        }
        let mut dom = VirtualDom::new(empty);
        let state = provide_state(&dom);
        dom.rebuild_in_place();
        let open = |name: &str| {
            let pool =
                DbPool::Sqlite(sqlx::sqlite::SqlitePool::connect_lazy("sqlite::memory:").unwrap());
            dom.in_runtime(|| {
                let mut registry = state.registry;
                let id = registry.write().insert(name, pool, WriteProtection::Open);
                id
            })
        };
        let (still_open, closed) = (open("a"), open("b"));
        let (end_open, ended_open) = tokio::sync::oneshot::channel::<()>();
        let (end_closed, ended_closed) = tokio::sync::oneshot::channel::<()>();
        dom.in_runtime(|| {
            state.mark_lost_when(
                still_open,
                async {
                    let _ = ended_open.await;
                },
                "a lost".into(),
            );
            state.mark_lost_when(
                closed,
                async {
                    let _ = ended_closed.await;
                },
                "b lost".into(),
            );
        });
        // Closing a tab ends its session; that is not news to report.
        dom.in_runtime(|| state.close_connection(closed, crate::ui::notice::Gesture::CLICK));
        drop(end_closed);
        drop(end_open);
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        while dom.in_runtime(|| state.lost_tunnels.peek().is_empty()) {
            assert!(tokio::time::Instant::now() < deadline, "never flagged");
            let _ = tokio::time::timeout(std::time::Duration::from_millis(50), dom.wait_for_work())
                .await;
            dom.render_immediate(&mut NoOpMutations);
        }
        // One more round, so the closed tab's watch has had its chance too.
        let _ =
            tokio::time::timeout(std::time::Duration::from_millis(50), dom.wait_for_work()).await;
        let lost = dom.in_runtime(|| state.lost_tunnels.peek().clone());
        assert_eq!(lost, HashMap::from([(still_open, "a lost".to_string())]));
    }
}
