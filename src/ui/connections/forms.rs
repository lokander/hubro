//! The add and edit form for a server connection (Postgres or SQL Server):
//! the fields and their per-engine [`ServerSpec`], the auth selector, the
//! "remember" box with its hint, and the submit pipeline that decides which
//! password is handed on and connects.

use dioxus::prelude::*;

use crate::config::{AuthMode, BackendKind, EditPrefill, SavedConnection, ServerAuth};
use crate::ui::classes::{BTN_CANCEL, BTN_PRIMARY, FORM_INPUT, FORM_SELECT};
use crate::ui::state::{ActiveView, AppState, Remember, ServerBackend};
use crate::ui::text_field::TextInput;

use super::prompts::remember_hint;
use super::tunnel::{tunnel_from_form, use_ssh_tunnel, SshTunnelFields, SshTunnelState};

/// The connection form's counterpart of [`remember_hint`]: what an unticked
/// box does to a password already in the keyring when the form's password
/// connects (`submit_server_form` hands it to the same
/// `stored_password_after_connect` decision the prompt does).
///
/// The form cannot know whether a stored copy exists without reading the
/// keyring as it opens, which could raise an unlock dialog for a form the user
/// may only be renaming, so wherever one can exist it gets the hedged line:
/// when editing a saved connection, and when `url` — the locator the fields
/// currently build, which is the account the keyring files a password under —
/// is a saved connection's locator. Adding a connection whose URL matches a
/// saved one is that saved connection as far as the keyring is concerned, and
/// a different password connecting unticked deletes its entry. Any other new
/// connection gets no line: nothing saved can have stored a password under its
/// locator. (`url` is `None` while the fields do not build one.)
///
/// Only when the submit hands a password to the connect — `handed`, the same
/// [`form_password`] verdict `submit_server_form` acts on: typed, pasted inside
/// the URL, or carried across from the old locator by an edit that moves it.
/// The carried case can delete too: moved onto another saved connection's
/// locator, the carried password connecting unticked deletes that
/// connection's differing stored one. With nothing handed on, the submit
/// connects through `connect_server`, which never deletes a stored password,
/// so a rename-only edit that keeps its locator is not warned about a deletion
/// it cannot make.
///
/// When editing moves the locator, the copy an unticked box costs is the
/// one under the *old* locator: `migrate_secret` drops it once a different
/// password has connected, ticked or not. Ticked, that is a move — the new
/// password is stored under the new locator — and unticked it is what this
/// line says.
fn form_remember_hint(
    editing: bool,
    handed: &FormPassword,
    url: Option<&str>,
    saved: &[SavedConnection],
) -> Option<&'static str> {
    use crate::ui::state::{PromptKind, StoredCopy};
    let saved_locator = url.is_some_and(|url| saved.iter().any(|entry| entry.locator() == url));
    if *handed != FormPassword::Nothing && (editing || saved_locator) {
        remember_hint(PromptKind::DbPassword, StoredCopy::Unknown)
    } else {
        None
    }
}

/// The locator [`ServerForm`]'s fields build for Postgres: what its submit connects,
/// saves, and keys the keyring under.
fn postgres_form_url(
    use_url: bool,
    pasted: &str,
    host: &str,
    port: &str,
    database: &str,
    user: &str,
    sslmode: &str,
) -> Result<String, crate::db::DbError> {
    if use_url {
        crate::db::normalize_pg_url(pasted)
    } else {
        crate::db::build_url(host, port, database, user, sslmode)
    }
}

/// The locator [`ServerForm`]'s fields build for SQL Server — the encrypt option and
/// the trust-server-certificate flag are SQL Server's alone. What its submit
/// connects, saves, and keys the keyring under.
#[allow(clippy::too_many_arguments)]
fn sqlserver_form_url(
    use_url: bool,
    pasted: &str,
    host: &str,
    port: &str,
    database: &str,
    user: &str,
    encrypt: &str,
    trust_cert: bool,
) -> Result<String, crate::db::DbError> {
    if use_url {
        crate::db::normalize_mssql_url(pasted)
    } else {
        crate::db::build_mssql_url(host, port, database, user, encrypt).map(|built| {
            if trust_cert {
                // build_mssql_url writes only the encrypt param; splice the
                // trust flag in after it. The URL it returned always parses.
                match url::Url::parse(&built) {
                    Ok(mut parsed) => {
                        parsed
                            .query_pairs_mut()
                            .append_pair("trustServerCertificate", "true");
                        String::from(parsed)
                    }
                    Err(_) => built,
                }
            } else {
                built
            }
        })
    }
}

/// The signals a [`ServerForm`] holds, handed to [`submit_server_form`] so
/// one pipeline can read the fields at submit time (signals are `Copy`).
#[derive(Clone, Copy)]
pub(super) struct ServerFormState {
    name: Signal<String>,
    host: Signal<String>,
    port: Signal<String>,
    database: Signal<String>,
    user: Signal<String>,
    /// `sslmode` for Postgres, `encrypt` for SQL Server.
    option: Signal<String>,
    /// Read only where the spec offers the checkbox.
    trust_cert: Signal<bool>,
    /// Never prefilled; empty means "keep whatever the keyring holds".
    password: Signal<String>,
    remember: Signal<Remember>,
    use_url: Signal<bool>,
    pasted_url: Signal<String>,
    auth_mode: Signal<AuthMode>,
    entra_tenant: Signal<String>,
    entra_client_id: Signal<String>,
    tunnel: SshTunnelState,
    form_error: Signal<Option<String>>,
}

impl ServerFormState {
    /// The fields the URL is built from, for the hint and the submit alike.
    /// Read during render, this subscribes the form to each of them, so the
    /// hint follows the typing; read from the submit's handler, no reactive
    /// scope is running and it subscribes nothing.
    fn fields(self) -> ServerFields {
        ServerFields {
            use_url: (self.use_url)(),
            pasted: (self.pasted_url)(),
            host: (self.host)(),
            port: (self.port)(),
            database: (self.database)(),
            user: (self.user)(),
            option: (self.option)(),
            trust_cert: (self.trust_cert)(),
        }
    }
}

/// The submit pipeline of [`ServerForm`], for both engines: validate the
/// tunnel fields, take the SSH passphrase and any password embedded in a
/// pasted URL, settle the display name and auth mode, register the pending
/// edit, carry a moved secret across, then dispatch the connect and close only
/// if it worked.
///
/// `built` is the URL the form's fields produced — `Ok` or the error to
/// show. It is computed by the caller, with [`ServerSpec::form_url`], because
/// that is the one step the engines genuinely differ in (which
/// `build_*`/`normalize_*` applies, and SQL Server's trust-server-certificate
/// flag), and the form's hint builds it the same way; building it is
/// side-effect free, so a tunnel-field error still surfaces first.
///
/// `old_locator` is set when editing a saved entry: the locator to replace,
/// which the edit may move.
fn submit_server_form(
    state: AppState,
    on_done: EventHandler<()>,
    spec: ServerSpec,
    form: ServerFormState,
    old_locator: Option<String>,
    built: Result<String, crate::db::DbError>,
) {
    use crate::tunnel::{TunnelAuth, TunnelConfig};
    // The engine connected is the one the form is for: derived here, so no
    // caller can hand the submit one engine's form and another's backend.
    let backend = spec.backend();
    let mut form_error = form.form_error;
    // Tunnel settings are validated first so a bad SSH field fails before any
    // connect attempt.
    let tunnel: Option<TunnelConfig> = match tunnel_from_form(
        *form.tunnel.use_tunnel.peek(),
        &form.tunnel.ssh_host.peek(),
        &form.tunnel.ssh_port.peek(),
        &form.tunnel.ssh_user.peek(),
        *form.tunnel.ssh_use_key.peek(),
        &form.tunnel.ssh_key_path.peek(),
    ) {
        Ok(tunnel) => tunnel,
        Err(err) => {
            form_error.set(Some(err));
            return;
        }
    };
    let entered_passphrase = if matches!(
        tunnel,
        Some(TunnelConfig {
            auth: TunnelAuth::KeyFile { .. },
            ..
        })
    ) {
        Some(form.tunnel.ssh_passphrase.peek().clone()).filter(|p| !p.is_empty())
    } else {
        None
    };
    // A password pasted inside the URL is used for this connect, like a typed
    // one, but never written into the saved locator.
    let embedded_password = embedded_url_password(*form.use_url.peek(), &form.pasted_url.peek());
    let url = match built {
        Ok(url) => url,
        Err(err) => {
            form_error.set(Some(err.to_string()));
            return;
        }
    };
    let display_name = display_name_for(&form.name.peek(), &url);
    // Authentication mode from the selector. Entra takes the
    // `user` field as the Entra principal; a token replaces the password.
    let auth: ServerAuth = match auth_from_form(
        *form.auth_mode.peek(),
        &form.entra_tenant.peek(),
        &form.entra_client_id.peek(),
    ) {
        Ok(auth) => auth,
        Err(err) => {
            form_error.set(Some(err));
            return;
        }
    };
    if matches!(auth, ServerAuth::Entra(_)) && entra_principal_missing(&url) {
        form_error.set(Some(
            "Entra needs a principal — fill the user field (e.g. you@contoso.com)".to_string(),
        ));
        return;
    }
    // The password this submit hands on, decided where the "remember" hint
    // decides too ([`form_password`]), so the two cannot disagree.
    let handed = form_password(
        &form.password.peek(),
        embedded_password,
        old_locator.as_deref(),
        &url,
    );
    // `None` when the box was not on screen: nobody was asked.
    let remember_choice = asks_remember(*form.auth_mode.peek()).then(|| *form.remember.peek());
    form_error.set(None);
    // spawn_forever: closing the modal (X or Escape) unmounts the form, and a
    // scope-tied `spawn` would be cancelled at its next await — abandoning the
    // connect with its reservation still held, which leaves the row stuck
    // showing progress it can no longer cancel.
    dioxus::core::spawn_forever(async move {
        // An edit is saved by whichever path confirms the connect — including
        // the Entra sign-in card, which resolves after this form has closed —
        // so the intent is registered up front rather than applied here.
        if let Some(old) = &old_locator {
            state.set_pending_edit(old.clone(), url.clone());
        }
        // "Leave the password empty to keep the existing one" has to keep
        // working when the edit moves the locator: the stored secret is still
        // filed under the OLD one, and the connect below looks up the new.
        // Carry it over first.
        let entered_password = match handed {
            FormPassword::Entered(password) => password,
            FormPassword::CarriedFrom(old) => crate::secrets::get_password_async(old)
                .await
                .ok()
                .flatten()
                .unwrap_or_default(),
            FormPassword::Nothing => String::new(),
        };
        // An entered passphrase seeds session memory so the tunnel open finds
        // it, and parks its "remember" answer for that open to redeem once it
        // accepts the passphrase, exactly as if both came from the prompt.
        // Parked set *or cleared*: an answer left by an earlier, abandoned
        // prompt would otherwise be redeemed against this passphrase.
        if let Some(passphrase) = &entered_passphrase {
            state.stash_ssh_passphrase(&url, passphrase.clone());
            state.set_ssh_remember(&url, remember_choice);
        }
        if matches!(auth, ServerAuth::Entra(_)) {
            // Entra: the connect either succeeds silently (and the flow saves
            // it) or raises the sign-in card (which saves on completion).
            // Either way, close the form — the card takes over.
            //
            // Closed *before* the await, not after: `on_done` is owned by the
            // connections screen, and a successful connect switches to the new
            // tab and unmounts it. This task outlives that (it is root-spawned),
            // so calling the handler afterwards would touch a dropped scope's
            // storage and panic. The Entra path yields after opening the tab
            // (it keeps the refresh token), so the unmount really does land
            // first.
            //
            // Guarded because ordering alone is not a guarantee: a *second*
            // focus-taking connect finishing mid-submit can unmount the screen
            // at any await here. The screen is mounted exactly when this view
            // is active, and when it is not, closing the form is a no-op anyway
            // — it went with the screen.
            close_form(&state, &on_done);
            state
                .connect_server(
                    backend,
                    url.clone(),
                    display_name.clone(),
                    tunnel.clone(),
                    auth,
                )
                .await;
            return;
        }
        if entered_password.is_empty() {
            // No password entered: try as-is (the keyring may hold one); an
            // auth failure raises the password prompt.
            state
                .connect_server(
                    backend,
                    url.clone(),
                    display_name.clone(),
                    tunnel.clone(),
                    ServerAuth::Password,
                )
                .await;
        } else {
            state
                .connect_server_with_password(
                    backend,
                    url.clone(),
                    display_name.clone(),
                    entered_password,
                    remember_choice,
                    tunnel.clone(),
                )
                .await;
        }
        // The connect flow saves on success (`save_server_if_open`), and the
        // tunnel open stores the passphrase, which is why neither happens
        // here; only close the form when the connection worked.
        if state.open_locators.peek().iter().any(|(_, l)| *l == url) {
            close_form(&state, &on_done);
        }
    });
}

/// Where the password a connection form's submit hands on comes from. See
/// [`form_password`].
#[derive(Debug, Clone, PartialEq, Eq)]
enum FormPassword {
    /// Typed into the field, or pasted inside the URL.
    Entered(String),
    /// None entered, and the edit moves the locator: the password stored
    /// under the old locator (this one) is read and handed on, so "leave it
    /// empty to keep the existing one" survives the move.
    CarriedFrom(String),
    /// None at all: the submit connects through `connect_server`, which never
    /// deletes a stored password.
    Nothing,
}

/// Decides what password a connection form's submit hands to
/// `connect_server_with_password` — where an unticked box can delete a stored
/// one — from the typed field, a password embedded in a pasted URL, the
/// locator being edited (`None` when adding) and the locator the fields build.
///
/// One function, called by both [`submit_server_form`] and the form's
/// "remember" hint, because a hint that re-derived this disagreed with the
/// submit: an edit moving onto another saved connection's locator with the
/// field empty carries the old password across and can delete that
/// connection's differing stored one, while the hint read the empty field and
/// said nothing.
fn form_password(
    typed: &str,
    embedded: Option<String>,
    old_locator: Option<&str>,
    url: &str,
) -> FormPassword {
    if !typed.is_empty() {
        return FormPassword::Entered(typed.to_string());
    }
    if let Some(embedded) = embedded {
        return FormPassword::Entered(embedded);
    }
    match old_locator {
        Some(old) if old != url => FormPassword::CarriedFrom(old.to_string()),
        _ => FormPassword::Nothing,
    }
}

/// Whether the connection form shows its "remember" box for `auth_mode`.
/// The box is rendered on this and its answer read on this, so a hidden box —
/// which keeps its default tick — is never taken for an answer.
fn asks_remember(auth_mode: AuthMode) -> bool {
    auth_mode == AuthMode::Password
}

/// The password pasted inside a URL, when the form is in paste mode. Used for
/// that connect exactly as a typed one — the "remember" box decides the
/// keyring — but never written into the saved locator, which is the
/// password-free normalization.
fn embedded_url_password(use_url: bool, pasted: &str) -> Option<String> {
    if !use_url {
        return None;
    }
    url::Url::parse(pasted.trim()).ok().and_then(|parsed| {
        // Url::password() returns the still-encoded form. Decoded lossily,
        // unlike the field prefills: a password is used as typed, and refusing
        // to decode it would silently connect with the percent-escapes.
        parsed.password().map(|p| {
            percent_encoding::percent_decode_str(p)
                .decode_utf8_lossy()
                .into_owned()
        })
    })
}

/// The name a submitted connection is saved and tabbed under: what the user
/// typed, or the URL-derived fallback when they left the field empty.
fn display_name_for(entered: &str, url: &str) -> String {
    let entered = entered.trim();
    if entered.is_empty() {
        default_server_name(url)
    } else {
        entered.to_string()
    }
}

/// Which server engine a [`ServerForm`] is for, and everything about the
/// form that differs by engine: the heading, the placeholders, the one option
/// select's choices and default, whether it offers to trust the server's
/// certificate, the submit's accent, and the URL its fields build.
///
/// Data rather than a slot for a per-engine element: the options it renders
/// are the ones [`ServerSpec::form_url`] reads, through the same
/// [`ServerFields`], so the URL the hint describes and the URL the submit
/// connects cannot be built from different fields.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ServerSpec {
    Postgres,
    SqlServer,
}

impl ServerSpec {
    /// The form for editing `saved`, or `None` for SQLite, which has no form
    /// (and is offered no Edit action). Matched on every engine, so a new one
    /// has to say which form edits it rather than falling into Postgres's.
    pub(super) fn editing(saved: &SavedConnection) -> Option<Self> {
        match saved.backend() {
            BackendKind::Postgres => Some(ServerSpec::Postgres),
            BackendKind::SqlServer => Some(ServerSpec::SqlServer),
            BackendKind::Sqlite => None,
        }
    }

    /// What the modal keys the form on: the engine, and the entry being
    /// edited. A change of either remounts the form instead of carrying one
    /// form's fields, and its "remember" answer, into another's.
    pub(super) fn form_key(self, edit: Option<&SavedConnection>) -> String {
        match edit {
            Some(saved) => format!("{self:?}:edit:{}", saved.locator()),
            None => format!("{self:?}:new"),
        }
    }

    fn kind(self) -> BackendKind {
        match self {
            ServerSpec::Postgres => BackendKind::Postgres,
            ServerSpec::SqlServer => BackendKind::SqlServer,
        }
    }

    fn backend(self) -> ServerBackend {
        match self {
            ServerSpec::Postgres => ServerBackend::POSTGRES,
            ServerSpec::SqlServer => ServerBackend::SQL_SERVER,
        }
    }

    fn url_placeholder(self) -> &'static str {
        match self {
            ServerSpec::Postgres => "postgres://user@host:5432/database?sslmode=require",
            ServerSpec::SqlServer => "mssql://user@host:1433/database?encrypt=on",
        }
    }

    fn port_placeholder(self) -> &'static str {
        match self {
            ServerSpec::Postgres => "5432",
            ServerSpec::SqlServer => "1433",
        }
    }

    /// The option select's values and labels: Postgres's `sslmode`, and SQL
    /// Server's `encrypt`, matching the URL params the backend accepts.
    fn options(self) -> &'static [(&'static str, &'static str)] {
        match self {
            ServerSpec::Postgres => &[
                ("prefer", "sslmode: prefer"),
                ("require", "sslmode: require"),
                ("disable", "sslmode: disable"),
            ],
            ServerSpec::SqlServer => &[
                ("on", "encrypt: on"),
                ("off", "encrypt: off (login only)"),
                ("plaintext", "encrypt: plaintext"),
            ],
        }
    }

    /// The option an added connection starts with, and an edited one whose
    /// URL names none.
    fn default_option(self) -> &'static str {
        match self {
            ServerSpec::Postgres => "prefer",
            ServerSpec::SqlServer => "on",
        }
    }

    /// Whether the form offers to accept the server's TLS certificate without
    /// CA validation — SQL Server's, for dev servers with self-signed certs
    /// (e.g. the stock Docker image).
    fn offers_trust_cert(self) -> bool {
        matches!(self, ServerSpec::SqlServer)
    }

    /// The submit button's fill, the engine's accent.
    fn submit_fill(self) -> &'static str {
        match self {
            ServerSpec::Postgres => "bg-cyan-700 hover:bg-cyan-600",
            ServerSpec::SqlServer => "bg-red-800 hover:bg-red-700",
        }
    }

    /// The SSH auth radios' group name, distinct per engine.
    pub(super) fn radio_group(self) -> &'static str {
        match self {
            ServerSpec::Postgres => "pg-ssh-auth",
            ServerSpec::SqlServer => "mssql-ssh-auth",
        }
    }

    /// The locator `fields` build: what the submit connects, saves, and keys
    /// the keyring under.
    fn form_url(self, fields: &ServerFields) -> Result<String, crate::db::DbError> {
        let ServerFields {
            use_url,
            pasted,
            host,
            port,
            database,
            user,
            option,
            trust_cert,
        } = fields;
        match self {
            ServerSpec::Postgres => {
                postgres_form_url(*use_url, pasted, host, port, database, user, option)
            }
            ServerSpec::SqlServer => sqlserver_form_url(
                *use_url,
                pasted,
                host,
                port,
                database,
                user,
                option,
                *trust_cert,
            ),
        }
    }
}

/// What a [`ServerForm`]'s URL is built from, read off its signals in one
/// place ([`ServerFormState::fields`]) for both the hint and the submit.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ServerFields {
    use_url: bool,
    pasted: String,
    host: String,
    port: String,
    database: String,
    user: String,
    /// `sslmode` for Postgres, `encrypt` for SQL Server.
    option: String,
    trust_cert: bool,
}

/// The add and edit form for a server connection, Postgres or SQL Server:
/// individual fields or a pasted URL, the engine's option select (and SQL
/// Server's trust-certificate box), the Auth dropdown, an optional SSH tunnel,
/// and the "remember" box. Everything that differs by engine is [`ServerSpec`]
/// data; the submit is [`submit_server_form`].
#[component]
pub(super) fn ServerForm(
    spec: ServerSpec,
    /// The saved entry being edited; `None` is the add flow. Its
    /// locator is the key to replace on save — the edit may move it.
    edit: Option<SavedConnection>,
    on_done: EventHandler<()>,
) -> Element {
    let state = use_context::<AppState>();
    let seed = edit
        .clone()
        .map(EditPrefill::from_saved)
        .unwrap_or_default();
    let old_locator = edit.as_ref().map(|e| e.locator().to_string());
    let hint_old_locator = old_locator.clone();
    let editing = edit.is_some();
    let form = ServerFormState {
        name: use_signal(|| seed.name.clone()),
        host: use_signal(|| seed.host.clone()),
        port: use_signal(|| seed.port.clone()),
        database: use_signal(|| seed.database.clone()),
        user: use_signal(|| seed.user.clone()),
        option: use_signal(|| {
            seed.option
                .clone()
                .unwrap_or_else(|| spec.default_option().to_string())
        }),
        trust_cert: use_signal(|| seed.trust_cert),
        // Never prefilled: the stored secret isn't shown, and an empty field
        // on save means "keep the existing keyring entry".
        password: use_signal(String::new),
        remember: use_signal(|| Remember::Keyring),
        use_url: use_signal(|| false),
        pasted_url: use_signal(String::new),
        // Authentication: a password unless the entry says otherwise.
        auth_mode: use_signal(|| seed.auth_mode),
        entra_tenant: use_signal(|| seed.entra_tenant.clone()),
        entra_client_id: use_signal(|| seed.entra_client_id.clone()),
        tunnel: use_ssh_tunnel(&seed),
        form_error: use_signal(|| Option::<String>::None),
    };
    let ServerFormState {
        mut name,
        mut host,
        mut port,
        mut database,
        mut user,
        option: mut engine_option,
        mut trust_cert,
        mut password,
        mut remember,
        mut use_url,
        mut pasted_url,
        auth_mode,
        entra_tenant,
        entra_client_id,
        tunnel,
        form_error,
    } = form;

    let submit = move || {
        // The URL is the spec's step; everything else is the shared pipeline.
        let built = spec.form_url(&form.fields());
        // Cloned per attempt: the submit takes ownership, and `submit` has to
        // stay FnMut.
        submit_server_form(state, on_done, spec, form, old_locator.clone(), built);
    };

    // Read at render, so the line follows the fields as they are typed — on
    // the same URL builder and the same password decision as the submit.
    let hint_url = spec.form_url(&form.fields()).ok();
    let remember_note = form_remember_hint(
        editing,
        &form_password(
            &password(),
            embedded_url_password(use_url(), &pasted_url()),
            hint_old_locator.as_deref(),
            hint_url.as_deref().unwrap_or_default(),
        ),
        hint_url.as_deref(),
        state.saved.read().entries(),
    );
    let label = spec.kind().label();
    rsx! {
        // Opaque, and a step above the page's slate-950: this renders over the
        // modal backdrop, and at 80% the connections list showed through it.
        div { class: "w-full max-w-xl rounded border border-slate-300 dark:border-slate-700 bg-slate-50 dark:bg-slate-900 p-4",
            div { class: "mb-3 flex items-center justify-between",
                span { class: "text-sm font-medium text-slate-900 dark:text-slate-200",
                    if editing { "Edit a {label} connection" } else { "Add a {label} connection" }
                }
                button {
                    class: "text-xs text-slate-500 dark:text-slate-400 hover:text-slate-900 dark:hover:text-slate-100",
                    onclick: move |_| {
                        let flip = !*use_url.read();
                        use_url.set(flip);
                    },
                    if use_url() { "Use individual fields" } else { "Paste a URL instead" }
                }
            }
            div { class: "flex flex-col gap-2",
                TextInput {
                    class: FORM_INPUT,
                    placeholder: "display name (optional)",
                    text: "{name}",
                    oninput: move |evt: FormEvent| name.set(evt.value()),
                }
                if use_url() {
                    TextInput {
                        class: FORM_INPUT,
                        placeholder: spec.url_placeholder(),
                        text: "{pasted_url}",
                        oninput: move |evt: FormEvent| pasted_url.set(evt.value()),
                    }
                } else {
                    div { class: "flex gap-2",
                        TextInput {
                            class: "{FORM_INPUT} flex-[3]",
                            placeholder: "host",
                            text: "{host}",
                            oninput: move |evt: FormEvent| host.set(evt.value()),
                        }
                        TextInput {
                            class: "{FORM_INPUT} flex-1",
                            placeholder: spec.port_placeholder(),
                            text: "{port}",
                            oninput: move |evt: FormEvent| port.set(evt.value()),
                        }
                    }
                    div { class: "flex gap-2",
                        TextInput {
                            class: FORM_INPUT,
                            placeholder: "database",
                            text: "{database}",
                            oninput: move |evt: FormEvent| database.set(evt.value()),
                        }
                        TextInput {
                            class: FORM_INPUT,
                            placeholder: "user",
                            text: "{user}",
                            oninput: move |evt: FormEvent| user.set(evt.value()),
                        }
                    }
                    div { class: "flex gap-2",
                        select {
                            class: FORM_SELECT,
                            onchange: move |evt| engine_option.set(evt.value()),
                            for (value , text) in spec.options().iter().copied() {
                                option {
                                    key: "{value}",
                                    value: "{value}",
                                    selected: *engine_option.read() == value,
                                    "{text}"
                                }
                            }
                        }
                    }
                    if spec.offers_trust_cert() {
                        label { class: "flex items-center gap-2 text-xs text-slate-500 dark:text-slate-400",
                            input {
                                r#type: "checkbox",
                                checked: trust_cert(),
                                onchange: move |evt| trust_cert.set(evt.checked()),
                            }
                            "Trust the server certificate (self-signed / dev servers)"
                        }
                    }
                }
                // Authentication mode. Entra uses the user field as the
                // principal and a token instead of a password.
                AuthModeFields { auth_mode, entra_tenant, entra_client_id }
                if auth_mode() == AuthMode::Password {
                    TextInput {
                        r#type: "password",
                        class: FORM_INPUT,
                        placeholder: if editing { "password (unchanged if left empty)" } else { "password" },
                        text: "{password}",
                        oninput: move |evt: FormEvent| password.set(evt.value()),
                    }
                }
                SshTunnelFields { tunnel, radio_group: spec.radio_group().to_string() }
                // Only meaningful for password auth — Entra asks on its
                // sign-in card.
                if asks_remember(auth_mode()) {
                    label { class: "flex items-center gap-2 text-xs text-slate-500 dark:text-slate-400",
                        input {
                            r#type: "checkbox",
                            checked: remember().checkbox(),
                            onchange: move |evt| remember.set(Remember::from_checkbox(evt.checked())),
                        }
                        "Remember in the system keyring (falls back to this session only)"
                    }
                    if let Some(hint) = remember_note {
                        p { class: "text-xs text-amber-700 dark:text-amber-400", "{hint}" }
                    }
                }
                div { class: "flex justify-end gap-2",
                    button {
                        class: BTN_CANCEL,
                        onclick: move |_| on_done.call(()),
                        "Cancel"
                    }
                    button {
                        class: "{BTN_PRIMARY} {spec.submit_fill()}",
                        onclick: move |_| submit(),
                        if editing { "Save & connect" } else { "Connect & save" }
                    }
                }
                if let Some(err) = form_error() {
                    p { class: "text-sm text-red-600 dark:text-red-400", "{err}" }
                }
            }
        }
    }
}

/// Closes a connection form from inside its submit task.
///
/// The task is root-spawned so closing the modal cannot abandon the connect,
/// which means it outlives the form — and `on_done` is a [`Callback`] owned by
/// the connections screen's scope, whose storage is freed when that scope is
/// dropped. Calling it then panics rather than no-oping (dioxus has no
/// `Callback::try_call`), and any connect that opens a focused tab drops the
/// screen. Calls are ordered to land before that happens; this is the backstop
/// for the case ordering cannot cover, a *sibling* connect finishing mid-submit.
///
/// `ActiveView::Connections` holds exactly while the screen is mounted, and
/// when it does not, the form is already gone with it.
fn close_form(state: &AppState, on_done: &EventHandler<()>) {
    if matches!(*state.active.peek(), ActiveView::Connections) {
        on_done.call(());
    }
}

/// Builds the saved [`ServerAuth`] from the form's [`AuthMode`] and its Entra
/// fields — the validation shared by both server forms.
fn auth_from_form(mode: AuthMode, tenant: &str, client_id: &str) -> Result<ServerAuth, String> {
    use crate::azure::EntraAuth;
    let client = client_id.trim().to_string();
    match mode {
        AuthMode::EntraInteractive => {
            let tenant = tenant.trim().to_string();
            if tenant.is_empty() {
                return Err("the Entra tenant must not be empty".to_string());
            }
            Ok(ServerAuth::Entra(EntraAuth::Interactive {
                tenant,
                client_id: (!client.is_empty()).then_some(client),
            }))
        }
        AuthMode::EntraManagedIdentity => Ok(ServerAuth::Entra(EntraAuth::ManagedIdentity {
            client_id: (!client.is_empty()).then_some(client),
        })),
        AuthMode::Password => Ok(ServerAuth::Password),
    }
}

/// Validation shared by both server forms: Entra needs the URL's
/// username as the principal — an empty one would fail later at the server
/// with an opaque error.
fn entra_principal_missing(url: &str) -> bool {
    url::Url::parse(url)
        .ok()
        .is_none_or(|u| u.username().is_empty())
}

/// The Auth selector's label for `mode`.
fn auth_mode_label(mode: AuthMode) -> &'static str {
    match mode {
        AuthMode::Password => "Auth: password",
        AuthMode::EntraInteractive => "Auth: Microsoft Entra ID (browser sign-in)",
        AuthMode::EntraManagedIdentity => "Auth: Microsoft Entra ID (managed identity)",
    }
}

/// The Auth dropdown (password / Entra interactive / Entra managed identity)
/// plus the Entra fields it reveals, in [`ServerForm`], which hands its
/// signals in (signals are `Copy`).
#[component]
fn AuthModeFields(
    auth_mode: Signal<AuthMode>,
    entra_tenant: Signal<String>,
    entra_client_id: Signal<String>,
) -> Element {
    rsx! {
        select {
            class: FORM_SELECT,
            onchange: move |evt| {
                if let Ok(mode) = evt.value().parse() {
                    auth_mode.set(mode);
                }
            },
            for mode in AuthMode::ALL {
                option {
                    value: mode.as_str(),
                    selected: auth_mode() == mode,
                    {auth_mode_label(mode)}
                }
            }
        }
        if auth_mode() == AuthMode::EntraInteractive {
            TextInput {
                class: FORM_INPUT,
                placeholder: "Entra tenant — id, domain, or 'organizations'",
                text: "{entra_tenant}",
                oninput: move |evt: FormEvent| entra_tenant.set(evt.value()),
            }
        }
        if auth_mode().is_entra() {
            TextInput {
                class: FORM_INPUT,
                placeholder: "application (client) ID — optional",
                text: "{entra_client_id}",
                oninput: move |evt: FormEvent| entra_client_id.set(evt.value()),
            }
            p { class: "text-xs text-slate-500 dark:text-slate-400",
                "The user field is your Entra principal (e.g. you@contoso.com); a token is used instead of a password."
            }
        }
    }
}

/// Fallback display name for a server connection URL: "database @ host".
/// Scheme-agnostic, so both engines' forms share it.
fn default_server_name(url: &str) -> String {
    match url::Url::parse(url) {
        Ok(parsed) => {
            let db = parsed.path().trim_start_matches('/');
            let host = parsed.host_str().unwrap_or("?");
            if db.is_empty() {
                host.to_string()
            } else {
                format!("{db} @ {host}")
            }
        }
        Err(_) => url.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::connections::test_support::{module_code, shipped_code_of};
    use std::path::PathBuf;

    #[test]
    fn the_form_hedges_about_a_stored_password_wherever_one_can_be_stored() {
        use crate::ui::state::{PromptKind, StoredCopy};
        let hedged = remember_hint(PromptKind::DbPassword, StoredCopy::Unknown);
        assert!(hedged.is_some());
        // Built the way the form builds them, so the match is on the exact
        // string the keyring files the password under.
        let typed = postgres_form_url(false, "", "db", "5432", "prod", "app", "prefer").unwrap();
        let pasted = postgres_form_url(
            true,
            "postgres://app:hunter2@DB/prod?sslmode=prefer",
            "",
            "",
            "",
            "",
            "",
        )
        .unwrap();
        let other = postgres_form_url(false, "", "db", "5432", "other", "app", "prefer").unwrap();
        let saved = [SavedConnection::Postgres {
            name: "prod".to_string(),
            url: typed.clone(),
            tunnel: None,
            auth: ServerAuth::Password,
            protection: Default::default(),
            color: None,
            group: None,
        }];
        // Each case goes through `form_password`, the decision the submit
        // acts on, so the hint is asked about what the submit would hand on.
        // (typed field, embedded password, edited locator, built URL)
        let (typed, pasted, other) = (typed.as_str(), pasted.as_str(), other.as_str());
        let cases = [
            (
                "x",
                None,
                Some(typed),
                Some(typed),
                hedged,
                "an edit handing a typed password",
            ),
            (
                "",
                None,
                Some(typed),
                Some(typed),
                None,
                "a rename-only edit hands nothing",
            ),
            (
                "",
                None,
                Some(other),
                Some(typed),
                hedged,
                "an edit moved onto a saved locator carries its old password across, \
                 which can delete that connection's differing stored one",
            ),
            (
                "",
                None,
                Some(typed),
                Some(other),
                hedged,
                "an edit moving off its locator carries the password across",
            ),
            (
                "x",
                None,
                None,
                Some(typed),
                hedged,
                "adding a saved URL again, typed",
            ),
            (
                "",
                Some("hunter2"),
                None,
                Some(pasted),
                hedged,
                "adding a saved URL again, pasted with its password",
            ),
            (
                "x",
                None,
                None,
                Some(other),
                None,
                "a new locator has nothing saved",
            ),
            (
                "",
                None,
                None,
                Some(typed),
                None,
                "adding with no password hands nothing",
            ),
            (
                "x",
                None,
                None,
                None,
                None,
                "fields that build no URL match nothing",
            ),
        ];
        for (field, embedded, old, url, expected, why) in cases {
            let handed = form_password(
                field,
                embedded.map(str::to_string),
                old,
                url.unwrap_or_default(),
            );
            assert_eq!(
                form_remember_hint(old.is_some(), &handed, url, &saved),
                expected,
                "{why}"
            );
        }
    }

    #[test]
    fn the_form_hands_on_one_password_decided_in_one_place() {
        use FormPassword::{CarriedFrom, Entered, Nothing};
        const OLD: &str = "postgres://app@db:5432/old?sslmode=prefer";
        const NEW: &str = "postgres://app@db:5432/new?sslmode=prefer";
        for (typed, embedded, old, url, expected, why) in [
            ("typed", None, None, NEW, Entered("typed".into()), "typed"),
            (
                "typed",
                Some("pasted"),
                Some(OLD),
                NEW,
                Entered("typed".into()),
                "the field outranks the URL and the carry",
            ),
            (
                "",
                Some("pasted"),
                None,
                NEW,
                Entered("pasted".into()),
                "embedded in a pasted URL",
            ),
            (
                "",
                None,
                Some(OLD),
                NEW,
                CarriedFrom(OLD.into()),
                "an edit moving the locator carries the old password across",
            ),
            (
                "",
                None,
                Some(OLD),
                OLD,
                Nothing,
                "an edit keeping its locator with the field empty hands nothing",
            ),
            ("", None, None, NEW, Nothing, "adding with nothing entered"),
        ] {
            assert_eq!(
                form_password(typed, embedded.map(str::to_string), old, url),
                expected,
                "{why}"
            );
        }
    }

    #[test]
    fn the_form_asks_to_remember_exactly_where_the_password_route_runs() {
        // The box is hidden under Entra but keeps its default tick, so its
        // value is an answer only where it is shown. The password route —
        // the one that hands the answer on — runs for whatever
        // `auth_from_form` maps to `Password`, so the two must agree for
        // every mode the selector offers.
        //
        // The selector offers every mode in `ALL` and nothing else; that
        // `ALL` lists every mode is
        // `auth_mode_all_lists_every_mode_once_in_declaration_order`'s.
        let selector = shipped_code_of("fn AuthModeFields(");
        assert!(
            selector.contains("for mode in AuthMode::ALL { option { value: mode.as_str(),"),
            "{selector}"
        );
        for mode in AuthMode::ALL {
            assert_eq!(
                asks_remember(mode),
                matches!(auth_from_form(mode, "tenant", ""), Ok(ServerAuth::Password)),
                "{mode:?}: the box's visibility and the password route disagree"
            );
        }
        assert!(asks_remember(AuthMode::Password));
        assert!(!asks_remember(AuthMode::EntraManagedIdentity));
    }

    #[test]
    fn each_engine_builds_its_own_url_from_the_shared_fields() {
        let fields = |option: &str, trust_cert: bool| ServerFields {
            use_url: false,
            pasted: String::new(),
            host: "db".to_string(),
            port: String::new(),
            database: "app".to_string(),
            user: "u".to_string(),
            option: option.to_string(),
            trust_cert,
        };
        let pg = ServerSpec::Postgres
            .form_url(&fields("require", false))
            .unwrap();
        assert_eq!(
            pg,
            postgres_form_url(false, "", "db", "", "app", "u", "require").unwrap()
        );
        // Postgres offers no trust box, so its tick (always unset from a
        // Postgres prefill) cannot reach the URL either way.
        assert!(!ServerSpec::Postgres.offers_trust_cert());
        assert_eq!(
            ServerSpec::Postgres
                .form_url(&fields("require", true))
                .unwrap(),
            pg
        );
        let ms = ServerSpec::SqlServer
            .form_url(&fields("off", true))
            .unwrap();
        assert_eq!(
            ms,
            sqlserver_form_url(false, "", "db", "", "app", "u", "off", true).unwrap()
        );
        assert!(ms.contains("trustServerCertificate=true"), "{ms}");
        assert!(!ServerSpec::SqlServer
            .form_url(&fields("off", false))
            .unwrap()
            .contains("trustServerCertificate"));
        // A pasted URL goes through the engine's own normalizer.
        let pasted = ServerFields {
            use_url: true,
            pasted: "mssql://sa@H:1433/master".to_string(),
            ..fields("on", false)
        };
        assert_eq!(
            ServerSpec::SqlServer.form_url(&pasted).unwrap(),
            crate::db::normalize_mssql_url("mssql://sa@H:1433/master").unwrap()
        );
        for spec in [ServerSpec::Postgres, ServerSpec::SqlServer] {
            assert_eq!(spec.backend().kind, spec.kind(), "{spec:?}");
            assert!(
                spec.options()
                    .iter()
                    .any(|(value, _)| *value == spec.default_option()),
                "{spec:?}: the default option is not one the select offers"
            );
        }
        assert_ne!(
            ServerSpec::Postgres.radio_group(),
            ServerSpec::SqlServer.radio_group()
        );
    }

    #[test]
    fn an_edit_opens_the_form_for_the_saved_engine() {
        let server = |kind: BackendKind| {
            let (name, url, tunnel, auth, protection, color, group) = (
                "x".to_string(),
                "u".to_string(),
                None,
                ServerAuth::Password,
                Default::default(),
                None,
                None,
            );
            match kind {
                BackendKind::SqlServer => SavedConnection::SqlServer {
                    name,
                    url,
                    tunnel,
                    auth,
                    protection,
                    color,
                    group,
                },
                _ => SavedConnection::Postgres {
                    name,
                    url,
                    tunnel,
                    auth,
                    protection,
                    color,
                    group,
                },
            }
        };
        assert_eq!(
            ServerSpec::editing(&server(BackendKind::SqlServer)),
            Some(ServerSpec::SqlServer)
        );
        assert_eq!(
            ServerSpec::editing(&server(BackendKind::Postgres)),
            Some(ServerSpec::Postgres)
        );
        let sqlite = SavedConnection::Sqlite {
            name: "x".to_string(),
            path: PathBuf::from("x.db"),
            protection: Default::default(),
            color: None,
            group: None,
        };
        assert_eq!(ServerSpec::editing(&sqlite), None);
    }

    #[test]
    fn the_form_is_keyed_on_its_engine_and_the_entry_it_edits() {
        let saved = |url: &str| SavedConnection::Postgres {
            name: "x".to_string(),
            url: url.to_string(),
            tunnel: None,
            auth: ServerAuth::Password,
            protection: Default::default(),
            color: None,
            group: None,
        };
        let (a, b) = (saved("postgres://u@a/db"), saved("postgres://u@b/db"));
        let keys = [
            ServerSpec::Postgres.form_key(None),
            ServerSpec::SqlServer.form_key(None),
            ServerSpec::Postgres.form_key(Some(&a)),
            ServerSpec::Postgres.form_key(Some(&b)),
        ];
        for (i, key) in keys.iter().enumerate() {
            for other in &keys[i + 1..] {
                assert_ne!(key, other);
            }
        }
        assert_eq!(
            ServerSpec::Postgres.form_key(Some(&a)),
            ServerSpec::Postgres.form_key(Some(&a))
        );
    }

    #[test]
    fn the_form_reads_its_remember_box_only_where_it_renders_it() {
        // The submit reads the box only where `asks_remember` says it is
        // shown. That is only an answer if the box *is* shown there: a render
        // gate turned round hides it under password auth, and its default tick
        // then stores the password and the passphrase unasked — with every
        // other test green. So the one form both engines use renders it once,
        // inside that gate.
        let form = "fn ServerForm(";
        let code = shipped_code_of(form);
        let gate = "if asks_remember(auth_mode()) { label {";
        assert_eq!(
            code.matches(gate).count(),
            1,
            "{form}: the box's gate: {code}"
        );
        assert_eq!(
            code.matches("remember()").count(),
            1,
            "{form}: the box is read somewhere besides its own `checked:`: {code}"
        );
        // The gate's first block is the box's `label`, and that block, by
        // brace matching, holds the box — not merely somewhere in the gate,
        // where it could sit in the hint instead.
        let label = code.find(gate).unwrap() + gate.len() - "label {".len();
        let end = label
            + crate::source_scan::block_end(&code[label..]).expect("the label's block closes");
        assert!(
            code[label..end].contains("checked: remember().checkbox(),"),
            "{form}: the box is not the checkbox in the label its gate opens, so \
             it renders outside the gate its answer is read under: {}",
            &code[label..end]
        );
        // And the submit's producer is the only read of the form's box, in
        // every file of the module.
        let shipped = crate::source_scan::code_only(&module_code());
        assert_eq!(shipped.matches("form.remember").count(), 1, "{shipped}");
        assert!(shipped.contains(
            "let remember_choice = asks_remember(*form.auth_mode.peek()).then(|| *form.remember.peek());"
        ));
    }

    #[test]
    fn an_embedded_password_is_only_taken_from_a_pasted_url() {
        // Field mode: the password field is the only source, even if the
        // (hidden) paste field still holds an old value.
        assert_eq!(
            embedded_url_password(false, "postgres://u:secret@h:5432/db"),
            None
        );
        assert_eq!(
            embedded_url_password(true, "postgres://u:secret@h:5432/db"),
            Some("secret".to_string())
        );
        // Percent escapes are what the user typed, so they are decoded before
        // the password is used.
        assert_eq!(
            embedded_url_password(true, "mssql://sa:p%40ss%20w%25rd@h:1433/db"),
            Some("p@ss w%rd".to_string())
        );
        // Surrounding whitespace is trimmed like the URL builders do.
        assert_eq!(
            embedded_url_password(true, "  postgres://u:secret@h/db  "),
            Some("secret".to_string())
        );
        // Nothing to take: no password in the URL, or nothing parseable.
        assert_eq!(embedded_url_password(true, "postgres://u@h:5432/db"), None);
        assert_eq!(embedded_url_password(true, "not a url"), None);
        assert_eq!(embedded_url_password(true, ""), None);
    }

    #[test]
    fn an_empty_name_field_falls_back_to_the_url() {
        assert_eq!(
            display_name_for("  ", "postgres://u@db.example.com:5432/app"),
            "app @ db.example.com"
        );
        assert_eq!(
            display_name_for("", "mssql://sa@db.example.com:1433/app"),
            "app @ db.example.com"
        );
        // A typed name wins, trimmed.
        assert_eq!(
            display_name_for("  prod  ", "postgres://u@db.example.com:5432/app"),
            "prod"
        );
    }
}
