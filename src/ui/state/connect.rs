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

use std::sync::Arc;

use crate::db::entra::{EntraTokens, RenewError, Renewed, Secret, Token};

/// Moves one keyring secret from `old` to `new`. Best-effort: a
/// missing secret, or a keyring that refuses, just means nothing to carry.
/// An existing secret under `new` is left alone — it came from the connect
/// that just succeeded and is therefore the more current one.
///
/// `validated` is what session memory holds under `new`: the secret the
/// connect confirming this edit just used. When it differs from the stored
/// one, the stored one is stale, and the connect has already decided the
/// keyring under `new` ([`stored_password_after_connect`]) — so nothing is
/// carried, and the old copy goes. Carrying it would put back the password a
/// "different password, unticked" connect had just declined to keep.
///
/// The session value is whatever last connected, asked or not: a different
/// password from the command line (which never decides the keyring) followed
/// by an edit that moves the locator drops the stored copy here too.
async fn migrate_secret(old: String, new: String, validated: Option<String>) {
    if old == new {
        return;
    }
    let Ok(Some(secret)) = crate::secrets::get_password_async(old.clone()).await else {
        return;
    };
    if validated.is_some_and(|validated| validated != secret) {
        let _ = crate::secrets::delete_password_async(old).await;
        return;
    }
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
/// one" bool it replaced.
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
/// accepts it. `None` means nobody was asked (the connection form hides its
/// box under Entra): any earlier answer is withdrawn and none recorded.
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
/// unticked the re-prompt for a passphrase typed into the connection form.
///
/// That form parks its answer here too. It used to stash the passphrase
/// without parking anything and store it itself once the connection opened,
/// so an answer parked by an earlier, abandoned prompt was still waiting: the
/// tunnel open redeemed it and stored the form's passphrase although its box
/// was unticked. Withdrawing on `None` closes that for a hidden box as well,
/// without recording a "no" the user never gave.
pub(super) fn park_ssh_remember(
    pending: &mut HashMap<String, bool>,
    url: &str,
    remember: Option<bool>,
) {
    match remember {
        Some(remember) => pending.insert(url.to_string(), remember),
        None => pending.remove(url),
    };
}

/// Withdraws the parked choice for a passphrase a tunnel open rejected, and
/// reports what the re-prompt's "remember" box should offer.
///
/// The withdrawal is the one [`park_ssh_remember`] argues for: the choice was
/// made about the secret that just failed, so leaving it parked would let some
/// later attempt redeem it. Reporting what it *was* is what keeps the
/// re-prompt honest — the box used to be re-ticked on every re-prompt, so a
/// user who cleared it and then mistyped had that decision undone by the typo.
///
/// An attempt with no answer on record defaults to offering the box ticked:
/// nothing has asked this session, so its prompt is a first prompt. That is
/// usually a passphrase read back from the keyring — but the rule is the
/// record, not the source. An answer left parked by an earlier attempt
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
/// The same key holds it in session memory when the user did not ask to be
/// remembered.
pub(crate) fn entra_secret_key(url: &str) -> String {
    format!("{url}#entra")
}

/// Where a kept Entra refresh token was found. A token lives in exactly one of
/// the two, and which one *is* the user's "remember" answer:
/// [`place_entra_refresh`] puts it in the keyring only when they ticked the
/// box, and takes it out of the other place either way.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum RefreshSource {
    Session,
    Keyring,
}

/// Whether a silent sign-in keeps the refresh token it gets back in the
/// keyring. No card was shown, so there is no fresh answer: the one the user
/// gave when they signed in stands, and the redeemed token's source records it.
///
/// A rotated token must not change homes on its own — out of the keyring it
/// would sign a remembered user out at the next start, and into it it would
/// store a sign-in the user declined to have remembered.
pub(super) fn redeemed_refresh_remembered(source: Option<RefreshSource>) -> bool {
    source == Some(RefreshSource::Keyring)
}

/// What the sign-in card offers after a silent attempt failed: unticked when
/// the token it tried was kept for the session only, because the user said
/// "don't remember" this run — a transient failure must not turn that into a
/// ticked box and a keyring write. Otherwise ticked, as the password prompt is.
pub(super) fn card_offers_remember(source: Option<RefreshSource>) -> bool {
    source != Some(RefreshSource::Session)
}

/// Whether a remembered sign-in is still in the keyring after a sign-in
/// attempt, from where the attempt found its refresh token (`source`) and
/// whether Entra refused that token for good (`rejected`, in which case
/// [`AppState::acquire_entra`] has just deleted it).
///
/// A session-kept token means the keyring holds none: a token lives in exactly
/// one of the two places ([`place_entra_refresh`] deletes the keyring copy when
/// it keeps one for the session). No token at all means the keyring read came
/// back empty or failed — the same accepted gap as [`StoredCopy::Absent`].
///
/// One case breaks the one-place rule: a `RefreshKeyring::Delete` that fails
/// leaves the token in session memory *and* the keyring. The next card then
/// reads `Absent` while an unticked box would still delete the keyring copy.
/// Accepted: [`AppState::keep_entra_refresh`] tells the user so when it
/// happens ("it will still sign you in").
pub(super) fn entra_stored_copy(source: Option<RefreshSource>, rejected: bool) -> StoredCopy {
    if source == Some(RefreshSource::Keyring) && !rejected {
        StoredCopy::Present
    } else {
        StoredCopy::Absent
    }
}

/// What to do with the keyring entry for a sign-in's refresh token.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum RefreshKeyring {
    /// Store this token there.
    Store(String),
    /// Delete whatever is there — an older remembered token outliving a
    /// "don't remember" answer would keep the user signed in regardless.
    Delete,
}

/// Puts a successful sign-in's refresh token where `remember` says: in session
/// memory and out of the keyring, or the reverse. Returns the keyring half,
/// which is I/O; the session half is done here.
///
/// The token is a long-lived credential — functionally a password — so it
/// reaches the keyring on the same answer a password does, and like a password
/// it is kept for the session either way.
pub(super) fn place_entra_refresh(
    session: &mut HashMap<String, String>,
    url: &str,
    refresh: String,
    remember: bool,
) -> RefreshKeyring {
    let key = entra_secret_key(url);
    if remember {
        session.remove(&key);
        RefreshKeyring::Store(refresh)
    } else {
        session.insert(key, refresh);
        RefreshKeyring::Delete
    }
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

/// The keys an edit that moves the locator carries across: [`secret_keys`]
/// without the Entra refresh token.
///
/// The token is left behind, and [`AppState::update_saved`] forgets it,
/// because where it is kept *is* the user's "remember" answer and the connect
/// confirming the edit has already placed its own token under the new locator.
/// Carrying the old one would apply an older answer over the new one: a
/// remembered token migrated into the keyring after a "don't remember", or a
/// session one carried next to a remembered token, whose next renewal then
/// reads as declined and deletes it.
pub(super) fn carried_secret_keys(locator: &str) -> [String; 2] {
    [locator.to_string(), ssh_secret_key(locator)]
}

/// Carries a connection's in-memory secrets, and its parked "remember the
/// passphrase" choice, across an edit that moved the locator — all but the
/// Entra refresh token ([`carried_secret_keys`]).
///
/// [`AppState::update_saved`] migrates the keyring entries for a reason that
/// applies just as much here: the locator is the key, so an edit that changes
/// host, port or database moves every session secret keyed by it. Left behind,
/// they are a passphrase and a password filed under a locator nothing will look
/// up again, while the connection they belong to re-prompts for secrets the app
/// is still holding.
///
/// **The parked "remember" choice deliberately does not move**, though it is
/// keyed by locator too, and an earlier report said it should. An edit lands
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
    for (old_key, new_key) in carried_secret_keys(old)
        .into_iter()
        .zip(carried_secret_keys(new))
    {
        let Some(secret) = session.remove(&old_key) else {
            continue;
        };
        // Never *over* a secret already under the new key: that one came from
        // the connect that just succeeded, and [`migrate_secret`] gives the
        // keyring copy the same precedence for the same reason. Overwriting it
        // would put an unvalidated secret where the next connect reads it as
        // previously accepted — so its failure prompts as if the stored copy
        // had gone stale, and the answer to that prompt can delete it.
        session.entry(new_key).or_insert(secret);
    }
}

/// Drops what a removed connection leaves behind in memory.
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

/// What a failed password connect does next, when it does anything beyond
/// reporting the error. See [`password_retry`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PasswordRetry {
    /// Nothing was remembered — a trust-auth attempt that turned out to need a
    /// password. Ask for one.
    Prompt,
    /// A remembered password was refused: drop it from session memory, then
    /// ask for a new one. The keyring copy stays until a *different* password
    /// connects — [`stored_password_after_connect`] decides it then.
    ForgetSessionAndPrompt,
}

/// Decides whether a failed server connect raises the password prompt, and
/// whether it first drops the remembered password from session memory.
///
/// Only [`DbError::CredentialsRejected`] qualifies. The backends build it from
/// the server's error code alone (SQL Server 18456, PostgreSQL `28P01`), so
/// nothing here reads message text. It used to: a match on "authentication
/// failed" in the message, which SQL Server's classifier wrote for a missing
/// or unopenable database too, since error 4060's text ends in "The login
/// failed." — so a database name that didn't exist deleted a working stored
/// password and then prompted for one that could not help.
///
/// Even the code is not proof the password is wrong: a Postgres role whose
/// `VALID UNTIL` has passed gets `28P01` for its correct password, and a SQL
/// Server contained-database user naming a missing database gets 18456. That
/// is why a rejection no longer deletes the keyring copy at all.
///
/// `had_password` says whether the attempt used a remembered password, and
/// [`AppState::connect_server`] only ever remembers passwords a connect
/// accepted. Without one, there is nothing to forget, and the rejection is a
/// trust-auth guess turning out wrong.
#[must_use = "the verdict decides whether the remembered password is dropped"]
pub(super) fn password_retry(err: &DbError, had_password: bool) -> Option<PasswordRetry> {
    if !err.credentials_rejected() {
        return None;
    }
    Some(if had_password {
        PasswordRetry::ForgetSessionAndPrompt
    } else {
        PasswordRetry::Prompt
    })
}

/// The credential a saved connection offers from memory, and whether that is a
/// remembered password — derived from the credential itself, so the two cannot
/// disagree. [`password_retry`] reads a refusal as the remembered password
/// failing on the strength of the flag, so a flag that said "remembered"
/// while the open sent nothing would prompt on every first use — the server
/// refuses the empty login, and the refusal is pinned on the password — and
/// an unticked answer to that prompt deletes the stored one.
fn remembered_credential(remembered: Option<&str>) -> (ServerCredential<'_>, bool) {
    let credential = match remembered {
        Some(password) => ServerCredential::Password(password),
        None => ServerCredential::None,
    };
    let offered = matches!(credential, ServerCredential::Password(_));
    (credential, offered)
}

/// What [`AppState::connect_server`]'s reads found out about a stored
/// password, for the prompt a refusal raises: `in_session` says session memory
/// supplied the password (so the keyring was never read), `found` that the
/// reads came back with one at all.
///
/// Only the keyring's own answer is knowledge. A session password says
/// nothing either way — one remembered from a ticked prompt is also stored, one
/// from the command line may sit beside a different stored one — and the
/// connect does not read the keyring a second time just to find out.
pub(super) fn stored_copy(in_session: bool, found: bool) -> StoredCopy {
    match (in_session, found) {
        (true, _) => StoredCopy::Unknown,
        (false, true) => StoredCopy::Present,
        (false, false) => StoredCopy::Absent,
    }
}

/// What a password connect that just succeeded does to the keyring entry for
/// its locator. See [`stored_password_after_connect`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum StoredPassword {
    /// Write the password that connected.
    Store,
    /// Delete the stored one: a different password connected, and the user
    /// declined to remember it.
    Delete,
    /// Leave the keyring as it is.
    Keep,
}

/// Decides what a successful password connect does to the stored password.
///
/// `remember` is the user's answer — `None` when nobody was asked, as for a
/// password off the command line. `stored` is what the keyring holds for the
/// locator, `None` when it holds nothing or cannot be read.
///
/// A refused password is never deleted on the refusal alone (see
/// [`password_retry`]), so this is where a stale one goes: once a *different*
/// password has connected, the stored one is demonstrably not the current
/// one. Ticked, the new one replaces it; unticked, it is deleted. The same
/// password connecting again leaves it, and so does a cancelled prompt, which
/// never gets here. Without an answer nothing changes, because deleting a
/// stored password is a decision the "remember" box takes.
#[must_use = "the verdict decides whether the keyring entry is written or deleted"]
pub(super) fn stored_password_after_connect(
    remember: Option<bool>,
    stored: Option<&str>,
    connected: &str,
) -> StoredPassword {
    match remember {
        Some(true) => StoredPassword::Store,
        Some(false) if stored.is_some_and(|stored| stored != connected) => StoredPassword::Delete,
        _ => StoredPassword::Keep,
    }
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
    EntraToken(Secret),
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
    aad_token: Option<Secret>,
}

/// The per-engine data the server connect flows are parameterized over.
///
/// Postgres and SQL Server connect through the same sequence — reserve, open
/// the tunnel, find a credential, open the pool, prompt or save — and used to
/// carry two line-for-line copies of it. They differ only in engine
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
                url: spliced(token.expose())?,
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
    ///
    /// `renewal` is an Entra sign-in's token provider: the pool asks it for the
    /// token each new connection logs in with, since it opens connections long
    /// after this first token expires ([`entra_renewal`]).
    async fn open(
        &self,
        connect_url: &str,
        credential: ServerCredential<'_>,
        tls_host: Option<&str>,
        renewal: Option<Arc<EntraTokens>>,
    ) -> Result<DbPool, DbError> {
        let ServerLogin { url, aad_token } = self.login(connect_url, credential)?;
        match self.kind {
            BackendKind::SqlServer => {
                let auth = match (aad_token, renewal) {
                    (Some(_), Some(tokens)) => MssqlAuth::Entra(tokens),
                    (Some(token), None) => MssqlAuth::AadToken(token),
                    (None, _) => MssqlAuth::Password,
                };
                DbPool::open_mssql_with(&url, &auth, tls_host).await
            }
            // No AAD login and no TLS host override exist here: `login` never
            // produces a token for Postgres (it goes into the URL), and sqlx
            // validates TLS against the URL's own host.
            _ => {
                debug_assert!(aad_token.is_none(), "a Postgres token belongs in the URL");
                match renewal {
                    Some(tokens) => DbPool::open_postgres_entra(&url, tokens).await,
                    None => DbPool::open_postgres(&url).await,
                }
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

/// The token provider an Entra connection's pool logs its later connections
/// in with ([`EntraTokens`]), seeded with the connect's own token.
///
/// It renews through [`azure::renew_silently`] alone, which has no browser to
/// open: a managed identity asks its endpoint, and an interactive sign-in
/// redeems `token.refresh_token` — the refresh token this very sign-in
/// returned, which the connect also keeps where the user's "remember" answer
/// says. The provider holds its copy (and any rotated one) in memory for as
/// long as the pool is open, the same class of keeping as session memory, and
/// writes nothing: no keyring store, no keyring delete. A refresh token
/// refused for good is dropped from the provider only; the keyring copy, if
/// any, is the next connect's to judge, as it is today.
fn entra_renewal(
    entra: &EntraAuth,
    resource: &'static str,
    token: &azure::AccessToken,
) -> Arc<EntraTokens> {
    let auth = entra.clone();
    let renew = Arc::new(move |refresh: Option<Secret>| {
        let auth = auth.clone();
        Box::pin(async move {
            azure::renew_silently(
                &auth,
                resource,
                refresh.as_ref(),
                &azure::Endpoints::default(),
            )
            .await
            .map(|renewed| Renewed {
                token: Token {
                    secret: renewed.secret,
                    expires_at: renewed.expires_at,
                },
                refresh: renewed.refresh_token,
            })
            .map_err(|err| {
                if azure::rejects_refresh(&err) {
                    RenewError::RefreshRejected(err.to_string())
                } else {
                    RenewError::Failed(err.to_string())
                }
            })
        }) as crate::db::entra::RenewFuture
    });
    Arc::new(EntraTokens::on_system_clock(
        Token {
            secret: token.secret.clone(),
            expires_at: token.expires_at,
        },
        token.refresh_token.clone(),
        matches!(entra, EntraAuth::ManagedIdentity { .. }),
        renew,
    ))
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

/// Carries a renamed group's fold across the rename.
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

/// Drops a deleted group's fold. A name nothing can expand again
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

    /// Marks a saved connection's write protection and accent colour,
    /// persists the list, and re-marks any tab already open on it
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

    /// Creates a connection group and persists the list. The error
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

    /// Files a saved connection under a group, or ungroups it with
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
    /// of `old_locator` rather than a new connection. Consumed by
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

    /// Applies an edit to a saved connection: overwrites the entry
    /// at `old_locator` — name included, which [`Self::save_server_if_open`]
    /// deliberately never does — and persists.
    ///
    /// When the edit changes host/port/database the normalized locator moves
    /// with it, and that locator keys the keyring account too. The stored
    /// secrets are carried across to the new key so an untouched password
    /// keeps working, then dropped from the old one; a secret already stored
    /// under the new locator (the connect that just succeeded wrote one) wins.
    /// The Entra refresh token is the exception: the old one is forgotten,
    /// never carried ([`carried_secret_keys`]).
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
            // The refresh token is not carried ([`carried_secret_keys`]): the
            // confirming connect already put its own where the user said. Its
            // session copy goes now, for the reason above; the keyring copy
            // in the spawn.
            self.session_passwords
                .write()
                .remove(&entra_secret_key(&old_locator));
            // Read after the carry: what the confirming connect validated, or
            // the old session copy moved across when it validated nothing.
            let moves: Vec<_> = carried_secret_keys(&old_locator)
                .into_iter()
                .zip(carried_secret_keys(&new_locator))
                .map(|(old, new)| {
                    let validated = self.session_passwords.peek().get(&new).cloned();
                    (old, new, validated)
                })
                .collect();
            spawn_forever(async move {
                self.forget_entra_refresh(&old_locator).await;
                for (old, new, validated) in moves {
                    migrate_secret(old, new, validated).await;
                }
            });
        }
    }

    /// Removes a saved connection and persists. Postgres and SQL Server
    /// entries also drop their keyring credentials (database password, SSH key
    /// passphrase, and cached Entra refresh token; deleting a missing entry is
    /// a no-op), and every entry drops the same keys from session memory along
    /// with any parked "remember the passphrase" choice.
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
            // and before the keyring work is spawned.
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
        let Some(config) = self.paths.peek().connections.clone() else {
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
        Result<azure::AccessToken, azure::AzureError>,
        Option<RefreshSource>,
        StoredCopy,
    ) {
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
        (acquired.token, source, stored)
    }

    /// Drops a connection's kept refresh token from session memory and the
    /// keyring alike.
    async fn forget_entra_refresh(mut self, url: &str) {
        let key = entra_secret_key(url);
        self.session_passwords.write().remove(&key);
        let _ = crate::secrets::delete_password_async(key).await;
    }

    /// Keeps a sign-in's refresh token where `remember` says
    /// ([`place_entra_refresh`]). A keyring that refuses the store leaves it
    /// for the session instead, and says so, as a refused password store does.
    async fn keep_entra_refresh(mut self, url: &str, refresh: String, remember: bool) {
        let placed =
            place_entra_refresh(&mut self.session_passwords.write(), url, refresh, remember);
        match placed {
            RefreshKeyring::Store(refresh) => {
                let key = entra_secret_key(url);
                let stored =
                    crate::secrets::store_password_async(key.clone(), refresh.clone()).await;
                if stored.is_err() {
                    self.session_passwords.write().insert(key, refresh);
                    self.connect_error.set(Some(
                        "connected, but the sign-in could not be stored in the system \
                         keyring — it is remembered for this session only"
                            .to_string(),
                    ));
                }
            }
            // Always deleted; the read only decides whether a failure is
            // worth saying, so a machine with no keyring at all — where
            // nothing can have been remembered — is not told one failed.
            RefreshKeyring::Delete => {
                let key = entra_secret_key(url);
                let held = crate::secrets::get_password_async(key.clone()).await;
                let deleted = crate::secrets::delete_password_async(key).await;
                if matches!(held, Ok(Some(_))) && deleted.is_err() {
                    self.connect_error.set(Some(
                        "connected, but an earlier remembered sign-in could not be removed \
                         from the system keyring — it will still sign you in"
                            .to_string(),
                    ));
                }
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
        token: azure::AccessToken,
        live_tunnel: Option<Tunnel>,
    ) {
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
                self.keep_entra_refresh(&url, refresh, remember).await;
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

    /// The write protection and accent colour saved for `locator`,
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

    /// Re-marks every open tab on `locator` after its saved entry is edited,
    /// so an open connection starts obeying a new protection
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
            include_str!("connect.rs"),
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
        for (file, source, calls) in [
            ("connect.rs", include_str!("connect.rs"), 2),
            ("connections.rs", include_str!("../connections.rs"), 0),
        ] {
            assert_eq!(
                code_of(&shipped(source))
                    .matches("persist_ssh_passphrase(")
                    .count(),
                calls,
                "{file} defines or calls persist_ssh_passphrase somewhere besides \
                 its definition and open_tunnel's gate"
            );
        }
    }

    /// The code of a function with its comment lines removed — the prose
    /// around these decisions discusses what is deliberately *not* stored, and
    /// would otherwise match every pattern the sweep below looks for.
    fn code_of(body: &str) -> String {
        body.lines()
            .filter(|line| !line.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// `code` with every run of whitespace collapsed to a single space, so a
    /// pinned line still matches once rustfmt wraps it. Matching the line as
    /// typed instead turns a reformat into a failure claiming the decision was
    /// severed from its policy — wrong, and alarming to whoever hits it.
    fn collapsed(code: &str) -> String {
        code.split_whitespace().collect::<Vec<_>>().join(" ")
    }

    /// The first tracked identifier that appears negated in `code`, skipping
    /// `!=` and macro bangs. Receivers are stepped over, so `!prompt.remember`
    /// and `!*remember.peek()` are found as readily as `!remember`.
    fn negated(code: &str, names: &[&str]) -> Option<String> {
        code.match_indices('!').find_map(|(at, _)| {
            let rest = code[at + 1..].trim_start_matches(['*', '&', '(']);
            let rest = ["prompt.", "self.", "state.", "acquired."]
                .iter()
                .find_map(|receiver| rest.strip_prefix(receiver))
                .unwrap_or(rest);
            names
                .iter()
                .find(|name| rest.starts_with(**name))
                .map(|name| format!("!{name}"))
        })
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
        let source = include_str!("connect.rs");
        let card = include_str!("../connections.rs");
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

    /// The calls of `macro_name` (`"format!("`, say) in `code`, each as the
    /// text between its parentheses.
    fn macro_calls(code: &str, macro_name: &str) -> Vec<String> {
        let mut calls = Vec::new();
        for (at, _) in code.match_indices(macro_name) {
            let preceded = code[..at]
                .chars()
                .next_back()
                .is_some_and(|c| c.is_alphanumeric() || c == '_');
            if preceded {
                continue;
            }
            let body = &code[at + macro_name.len()..];
            let (mut depth, mut in_str, mut escaped) = (1usize, false, false);
            for (offset, ch) in body.char_indices() {
                match (in_str, ch) {
                    (true, '\\') if !escaped => {
                        escaped = true;
                        continue;
                    }
                    (true, '"') if !escaped => in_str = false,
                    (false, '"') => in_str = true,
                    (false, '(') => depth += 1,
                    (false, ')') => {
                        depth -= 1;
                        if depth == 0 {
                            calls.push(body[..offset].to_string());
                            break;
                        }
                    }
                    _ => {}
                }
                escaped = false;
            }
        }
        calls
    }

    /// What of a macro call can name a binding: the code outside its string
    /// literals, and the `{…}` placeholders inside them. A literal's own words
    /// ("token endpoint") are not a value being formatted.
    fn formatted_names(call: &str) -> String {
        let (mut out, mut in_str, mut in_hole, mut escaped) = (String::new(), false, false, false);
        for ch in call.chars() {
            if in_str {
                match ch {
                    '\\' if !escaped => {
                        escaped = true;
                        continue;
                    }
                    '"' if !escaped => in_str = false,
                    '{' => in_hole = true,
                    '}' => {
                        in_hole = false;
                        out.push(' ');
                    }
                    _ if in_hole => out.push(ch),
                    _ => {}
                }
                escaped = false;
            } else if ch == '"' {
                in_str = true;
            } else {
                out.push(ch);
            }
        }
        out
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
        let connect = include_str!("connect.rs");
        let azure = include_str!("../../azure.rs");
        let postgres = include_str!("../../db/postgres.rs");
        let mssql = include_str!("../../db/sqlserver/pool.rs");
        let registry = include_str!("../../db/registry.rs");
        let provider = include_str!("../../db/entra.rs")
            .split("#[cfg(test)]")
            .next()
            .expect("the shipped half")
            .to_string();
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

    /// The renewal the connect builds for an interactive sign-in that kept no
    /// refresh token never renews: an expired token is refused at once, with
    /// no renewal attempted (which would carry its error in the message).
    #[tokio::test]
    async fn an_interactive_sign_in_without_a_refresh_token_is_never_renewed() {
        let token = azure::AccessToken {
            secret: Secret::new("expired"),
            expires_at: std::time::SystemTime::now() - std::time::Duration::from_secs(1),
            refresh_token: None,
        };
        let tokens = entra_renewal(
            &EntraAuth::interactive_default(),
            azure::SQLDB_RESOURCE,
            &token,
        );
        let refused = tokens.current().await.unwrap_err().to_string();
        assert!(refused.contains("reconnect to sign in again"), "{refused}");
        assert!(
            !refused.contains("Entra sign-in:"),
            "a renewal was attempted: {refused}"
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
        let source = include_str!("connect.rs");
        let card = method_body(include_str!("../connections.rs"), "fn EntraSignInCard(");
        let signin = method_body(source, "pub async fn connect_server_with_entra_signin(");
        let silent = method_body(source, "async fn connect_server_entra(");
        let finish = method_body(source, "async fn finish_entra_connect(");
        let acquire = method_body(source, "async fn acquire_entra(");
        let keep = method_body(source, "async fn keep_entra_refresh(");
        let cached = method_body(source, "async fn cached_entra_refresh(");
        let forget = method_body(source, "async fn forget_entra_refresh(");
        let restore = method_body(include_str!("session.rs"), "async fn connects_silently(");
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
                "self.keep_entra_refresh(&url, refresh, remember).await;",
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
        // And forgetting a refused token clears both homes.
        let forgets = collapsed(&code_of(&forget));
        assert!(
            forgets.contains("self.session_passwords.write().remove(&key);")
                && forgets.contains("crate::secrets::delete_password_async(key)"),
            "a refused token survives in one of its homes: {forgets}"
        );

        // Each keyring call sits in the arm of the verdict that asks for it:
        // swapping them stores a declined sign-in and deletes a remembered one
        // while every binding above still reads correctly.
        let store = method_body(&keep, "RefreshKeyring::Store(refresh) => {");
        assert!(
            store.contains("store_password_async") && !store.contains("delete_password_async"),
            "{store}"
        );
        let delete = method_body(&keep, "RefreshKeyring::Delete => {");
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
            moved[..spawned_at].contains(
                "self.session_passwords .write() .remove(&entra_secret_key(&old_locator));"
            ) || moved[..spawned_at].contains(
                "self.session_passwords.write().remove(&entra_secret_key(&old_locator));"
            ),
            "the old locator's session token is not dropped before the spawn, \
             so it outlives the edit by three keyring round trips: {moved}"
        );
        assert!(
            moved.contains("self.forget_entra_refresh(&old_locator).await;")
                && moved.contains("carried_secret_keys(&old_locator)")
                && moved.contains("carried_secret_keys(&new_locator)")
                && ident_count(&moved, "secret_keys") == 0,
            "an edit carries the refresh token across, or forgets the wrong \
             one: {moved}"
        );

        // Every route to the token's key, so a new write or read cannot join
        // the wire without being named here. Removal reaches it through
        // `secret_keys`, which deletes all three keys; an edit drops it,
        // above.
        let shipped_code = code_only(&shipped(source));
        let named: usize = [
            "fn entra_secret_key(",
            "fn secret_keys(",
            "fn place_entra_refresh(",
            "async fn cached_entra_refresh(",
            "async fn forget_entra_refresh(",
            "async fn keep_entra_refresh(",
            "pub fn update_saved(",
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
        for other in [
            include_str!("session.rs"),
            include_str!("mod.rs"),
            include_str!("../connections.rs"),
        ] {
            assert_eq!(
                ident_count(&code_only(&shipped(other)), "entra_secret_key"),
                0
            );
        }
    }

    #[test]
    fn a_silent_sign_in_keeps_its_token_where_the_user_put_it() {
        // Only a remembered token is in the keyring, so its rotation stays
        // there; one kept for the session stays for the session. No source
        // (managed identity, which returns no refresh token) is never a
        // reason to start remembering.
        assert!(redeemed_refresh_remembered(Some(RefreshSource::Keyring)));
        assert!(!redeemed_refresh_remembered(Some(RefreshSource::Session)));
        assert!(!redeemed_refresh_remembered(None));

        // The card after a failed silent attempt: a "don't remember" given
        // this run stands, anything else is offered ticked.
        assert!(!card_offers_remember(Some(RefreshSource::Session)));
        assert!(card_offers_remember(Some(RefreshSource::Keyring)));
        assert!(card_offers_remember(None));
    }

    #[test]
    fn a_refresh_token_reaches_the_keyring_only_when_remembered() {
        let url = "postgres://u@h:5432/db";
        let key = entra_secret_key(url);
        let other = entra_secret_key("postgres://u@other:5432/db");

        // Remembered: to the keyring, and out of session memory, so the
        // token has one home and the next silent sign-in reads it as
        // remembered.
        let mut session = HashMap::from([
            (key.clone(), "older".to_string()),
            (other.clone(), "theirs".to_string()),
        ]);
        assert_eq!(
            place_entra_refresh(&mut session, url, "fresh".to_string(), true),
            RefreshKeyring::Store("fresh".to_string())
        );
        assert!(!session.contains_key(&key));

        // Declined: for the session only, and any remembered one is deleted,
        // since keeping it would sign the user in at the next start anyway.
        assert_eq!(
            place_entra_refresh(&mut session, url, "fresh".to_string(), false),
            RefreshKeyring::Delete
        );
        assert_eq!(session.get(&key).map(String::as_str), Some("fresh"));

        assert_eq!(
            session.get(&other).map(String::as_str),
            Some("theirs"),
            "placing one connection's token touched another's"
        );
    }

    #[test]
    fn a_parked_choice_decides_whether_the_passphrase_is_stored() {
        // The policy itself, executed rather than read. The tests around it
        // pin *where* the write happens; these pin *what it decides*, which is
        // the half that can be inverted without moving a single call.
        //
        // Both polarities are defects, and the second is the worse one: losing
        // the choice is a bug this has had before, but storing a secret the
        // user declined is a privacy failure the app never mentions.
        let url = "postgres://u@h:5432/db";
        let mut pending = HashMap::new();

        // Ticked, then accepted from a passphrase just typed: stored.
        park_ssh_remember(&mut pending, url, Some(true));
        assert!(redeem_ssh_remember(
            &mut pending,
            url,
            Some(PassphraseSource::Session)
        ));

        // Unticked: never stored, however the passphrase was accepted.
        park_ssh_remember(&mut pending, url, Some(false));
        assert!(!redeem_ssh_remember(
            &mut pending,
            url,
            Some(PassphraseSource::Session)
        ));

        // An earlier tick must not survive a later untick — the intent is
        // keyed by locator and outlives the attempt that made it.
        park_ssh_remember(&mut pending, url, Some(true));
        park_ssh_remember(&mut pending, url, Some(false));
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

        // Nobody asked (the form's box hidden): an earlier tick is withdrawn,
        // so it cannot store this passphrase — and no "no" is recorded in its
        // place, so a re-prompt is a first prompt and offers the box ticked.
        park_ssh_remember(&mut pending, url, Some(true));
        park_ssh_remember(&mut pending, url, None);
        assert!(
            !redeem_ssh_remember(&mut pending, url, Some(PassphraseSource::Session)),
            "an answer left by an earlier attempt survived an unasked one"
        );
        park_ssh_remember(&mut pending, url, Some(false));
        park_ssh_remember(&mut pending, url, None);
        assert!(
            withdraw_ssh_remember(&mut pending, url),
            "an unasked answer was recorded as a no"
        );
    }

    #[test]
    fn a_redeemed_choice_is_consumed_and_never_rewrites_the_keyring() {
        let url = "postgres://u@h:5432/db";

        // One tick, one write: the next acceptance must not store again.
        let mut pending = HashMap::new();
        park_ssh_remember(&mut pending, url, Some(true));
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
        park_ssh_remember(&mut pending, url, Some(true));
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
        park_ssh_remember(&mut pending, url, Some(true));
        assert!(!redeem_ssh_remember(&mut pending, url, None));
        assert!(pending.is_empty());

        // Other locators are untouched throughout.
        let mut pending = HashMap::new();
        park_ssh_remember(&mut pending, url, Some(true));
        park_ssh_remember(&mut pending, "postgres://u@other:5432/db", Some(true));
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
    /// The `#entra` one is a refresh token from a sign-in the user did not ask
    /// to have remembered ([`place_entra_refresh`]).
    fn session_for(locator: &str) -> HashMap<String, String> {
        HashMap::from([
            (locator.to_string(), "db-password".to_string()),
            (ssh_secret_key(locator), "letmein".to_string()),
            (entra_secret_key(locator), "refresh-token".to_string()),
        ])
    }

    #[test]
    fn an_edited_locator_carries_its_session_secrets_with_it() {
        // The keyring entries already move — `update_saved` migrates
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
        // The refresh token is never carried: where it is kept is the
        // user's answer, and the confirming connect has placed its own.
        // `update_saved` forgets the old one, keyring copy included.
        assert_eq!(
            session.get(entra_secret_key(NEW).as_str()),
            None,
            "the edit carried a refresh token next to the one the confirming \
             connect placed by the user's answer"
        );
        // Nothing carried may be left under the old locator: it names no
        // connection now, and a later connection reusing that URL would
        // inherit it.
        for key in carried_secret_keys(OLD) {
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
        // session hit as good, fails, and prompts as if the keyring copy had
        // gone stale, so a stored secret is put at risk by one nobody
        // validated.
        //
        // `migrate_secret` gives the keyring copy the same precedence, in so
        // many words, so this is also the two stores agreeing rather than
        // drifting.
        const OLD: &str = "postgres://u@old:5432/db";
        const NEW: &str = "postgres://u@new:5432/db";
        // Every key, not just the bare locator: the passphrase is filed under
        // the same moved locator and reaches the same "read a session hit as
        // already accepted" path, so a rule that held for the password alone
        // would be half a fix. (The Entra token is never carried at all.)
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
        for key in carried_secret_keys(OLD) {
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
        // `remove_saved` deletes the three keyring entries; their
        // session-lived counterparts stayed. The parked intent is again the
        // sharp edge — recreating the same URL in the same session would
        // redeem it and write back the passphrase the deletion just removed.
        const URL: &str = "postgres://u@h:5432/db";
        let mut session = session_for(URL);
        let mut pending = HashMap::new();
        park_ssh_remember(&mut pending, URL, Some(true));

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
        park_ssh_remember(&mut pending, URL, Some(true));
        forget_connection_secrets(&mut session_for(URL), &mut pending, URL);
        assert!(
            withdraw_ssh_remember(&mut pending, URL),
            "the deleted connection left an answer behind, so recreating it \
             offers an unticked box the user never cleared"
        );
        assert!(pending.is_empty(), "the deletion left an entry behind");

        let mut session = session_for(URL);
        let mut pending = HashMap::new();
        park_ssh_remember(&mut pending, URL, Some(true));
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
        park_ssh_remember(&mut pending, OTHER, Some(true));
        forget_connection_secrets(&mut session, &mut pending, URL);
        assert_eq!(session, session_for(OTHER));
        assert!(pending.contains_key(OTHER));
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
            include_str!("connect.rs"),
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
            include_str!("connect.rs"),
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
        // `remember` was `use_signal(|| true)`, so every re-prompt
        // arrived freshly ticked: unticking the box and then mistyping the
        // passphrase silently restored the decision to store it.
        const URL: &str = "postgres://u@h:5432/db";

        // Unticked, typed, rejected: the re-prompt still offers unticked.
        let mut pending = HashMap::new();
        park_ssh_remember(&mut pending, URL, Some(false));
        assert!(
            !withdraw_ssh_remember(&mut pending, URL),
            "a mistyped passphrase re-ticked a box the user had cleared"
        );

        // Ticked, typed, rejected: still ticked, and the intent is withdrawn
        // either way — it was made about the passphrase that just failed.
        let mut pending = HashMap::new();
        park_ssh_remember(&mut pending, URL, Some(true));
        assert!(withdraw_ssh_remember(&mut pending, URL));
        assert!(
            !redeem_ssh_remember(&mut pending, URL, Some(PassphraseSource::Session)),
            "the choice for a rejected passphrase stayed parked, where a later \
             attempt can redeem it"
        );

        // Never asked at all: a passphrase read back from the keyring, or one
        // typed into the connection form while its box was hidden — which
        // parks no answer. Both must offer the default, and "no
        // answer" is only distinguishable from "answered no" because the
        // answer itself is stored. While this was a `HashSet` the two were the
        // same state, which silently unticked the form user's re-prompt.
        let mut pending = HashMap::new();
        assert!(
            withdraw_ssh_remember(&mut pending, URL),
            "a first prompt for this connection came up unticked"
        );
        park_ssh_remember(&mut pending, "postgres://u@other:5432/db", Some(false));
        assert!(
            withdraw_ssh_remember(&mut pending, URL),
            "another connection's declined choice decided this prompt"
        );

        // Another connection's parked choice is not withdrawn with it.
        let mut pending = HashMap::new();
        park_ssh_remember(&mut pending, "postgres://u@other:5432/db", Some(true));
        let _ = withdraw_ssh_remember(&mut pending, URL);
        assert_eq!(pending.len(), 1);
    }

    #[test]
    fn only_a_keyring_sourced_passphrase_is_deleted_when_it_is_rejected() {
        const KEY: &str = "postgres://u@h:5432/db#ssh";

        // Typed at the prompt and rejected: the keyring copy is somebody
        // else's — the *saved* passphrase, which this attempt says nothing
        // about. Deleting it here is the bug.
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
    fn a_backend_descriptor_carries_its_engine_s_data() {
        // The whole point of the descriptor: every per-engine
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
        // be told the server's real hostname.
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
            .login(PG, ServerCredential::EntraToken(Secret::new(TOKEN)))
            .unwrap();
        assert_eq!(login.url, format!("postgres://u:{TOKEN}@h:5432/db"));
        assert!(
            login.aad_token.is_none(),
            "Postgres has no driver-side token login to route to"
        );

        // SQL Server does: the token goes to the driver and the URL is left
        // untouched — a token in the URL would be sent as a password.
        let login = ms
            .login(MS, ServerCredential::EntraToken(Secret::new(TOKEN)))
            .unwrap();
        assert_eq!(login.url, MS, "the URL must be unchanged");
        assert!(
            !login.url.contains(TOKEN),
            "the access token must never reach the URL on SQL Server"
        );
        assert_eq!(login.aad_token.as_ref().map(Secret::expose), Some(TOKEN));
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
            .login("not a url", ServerCredential::EntraToken(Secret::new("t")))
            .is_err());
        // …while a SQL Server token never touches the URL, which is the whole
        // point: nothing can fail at this step.
        assert!(ServerBackend::SQL_SERVER
            .login("not a url", ServerCredential::EntraToken(Secret::new("t")))
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
        // list, never inferred by a connect.
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

    /// One of every [`DbError`] variant. The match is the point: a new variant
    /// does not compile here until someone says which side of the password
    /// decision it falls on.
    fn every_db_error(message: &str) -> Vec<DbError> {
        let m = || message.to_string();
        let all = vec![
            DbError::Connect(m()),
            DbError::Query(m()),
            DbError::Introspect(m()),
            DbError::RowCountMismatch(m()),
            DbError::Unsupported(m()),
            DbError::Transient(m()),
            DbError::CredentialsRejected(m()),
        ];
        for err in &all {
            match err {
                DbError::Connect(_)
                | DbError::Query(_)
                | DbError::Introspect(_)
                | DbError::RowCountMismatch(_)
                | DbError::Unsupported(_)
                | DbError::Transient(_)
                | DbError::CredentialsRejected(_) => {}
            }
        }
        all
    }

    #[test]
    fn only_a_credential_rejection_prompts_and_only_a_remembered_one_is_forgotten() {
        // Every wording a text rule could key on, each under every variant:
        // whatever the message says, only the variant decides. The first is
        // the message SQL Server's classifier used to produce for error 4060
        // — a database that does not exist — whose text match deleted the
        // stored password.
        let messages = [
            "authentication failed — Token error: 'Cannot open database \"no_such_db\" \
             requested by the login. The login failed.' on server x executing  on line 1 \
             (code: 4060, state: 1, class: 11)",
            // The 4060 label as it reads now.
            "cannot open the database — it may not exist, be offline or restoring, or this \
             login may lack access to it; the credentials were accepted — Token error: \
             'Cannot open database \"x\" requested by the login. The login failed.'",
            "Login failed for user 'x'.",
            "authentication failed — error returned from database: password authentication \
             failed for user \"x\"",
            "anything else",
        ];
        for message in messages {
            for err in every_db_error(message) {
                let rejected = matches!(err, DbError::CredentialsRejected(_));
                assert_eq!(
                    password_retry(&err, true),
                    rejected.then_some(PasswordRetry::ForgetSessionAndPrompt),
                    "{err:?} with a remembered password"
                );
                // Nothing remembered, nothing to forget — but the prompt is
                // still how a trust-auth guess learns it needed a password.
                assert_eq!(
                    password_retry(&err, false),
                    rejected.then_some(PasswordRetry::Prompt),
                    "{err:?} with no remembered password"
                );
            }
        }
    }

    #[test]
    fn a_remembered_password_is_both_what_is_offered_and_what_is_reported() {
        // The flag is what licenses the delete, so it must describe the
        // credential actually sent: "remembered" with nothing offered turns the
        // server's refusal of an empty login into the deletion of a good
        // password.
        let (credential, remembered) = remembered_credential(Some("s3cret"));
        assert!(matches!(credential, ServerCredential::Password("s3cret")));
        assert!(remembered, "a remembered password that was offered");
        // An empty remembered password is still one that was offered.
        let (credential, remembered) = remembered_credential(Some(""));
        assert!(matches!(credential, ServerCredential::Password("")));
        assert!(remembered);
        // Nothing remembered — including a keyring read that failed, which the
        // caller reads as nothing: nothing is offered, and a refusal can only
        // prompt.
        let (credential, remembered) = remembered_credential(None);
        assert!(matches!(credential, ServerCredential::None));
        assert!(
            !remembered,
            "nothing was offered, so nothing may be forgotten"
        );
    }

    /// `line` without a trailing `//` comment. A `//` inside a string literal
    /// (a URL) is kept: it counts as a comment only where the quotes before it
    /// are balanced. Approximate — escaped quotes and the `'"'` char literal
    /// are skipped, but quotes are tracked per line, so a continuation line of
    /// a multi-line string that holds `//` is cut there.
    ///
    /// **It fails open.** Cutting too much drops real code, which weakens the
    /// must-not-contain checks it feeds (nothing outside the gate, no message
    /// text under `src/ui`) instead of tripping them; the ordered and verbatim
    /// pins would fail loudly. Enough for the shipped code it reads today.
    fn strip_trailing_comment(line: &str) -> &str {
        let bytes = line.as_bytes();
        let mut in_string = false;
        let mut i = 0;
        while i < bytes.len() {
            match bytes[i] {
                b'\\' if in_string => i += 1,
                b'\'' if !in_string && bytes.get(i + 1..i + 3) == Some(b"\"'") => i += 2,
                b'"' => in_string = !in_string,
                b'/' if !in_string && bytes.get(i + 1) == Some(&b'/') => return &line[..i],
                _ => {}
            }
            i += 1;
        }
        line
    }

    /// `source` without comments — whole comment lines and trailing ones — so
    /// a test reading code cannot be satisfied, or tripped, by prose that names
    /// what it looks for.
    fn code_only(source: &str) -> String {
        source
            .lines()
            .map(strip_trailing_comment)
            .filter(|line| !line.trim().is_empty())
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// `source` up to its test module (`#[cfg(test)]` followed by `mod`): the
    /// shipped code alone. A `#[cfg(test)]` on anything else — `cli.rs` has
    /// one on a `use` — does not end it. Line-based, so a CRLF checkout reads
    /// the same.
    fn shipped(source: &str) -> String {
        let lines: Vec<&str> = source.lines().collect();
        let end = lines
            .windows(2)
            .position(|w| w[0].trim() == "#[cfg(test)]" && w[1].trim_start().starts_with("mod "))
            .unwrap_or(lines.len());
        lines[..end].join("\n")
    }

    /// `code` with the contents of every `"…"` literal removed (escapes
    /// honoured), so counting identifiers does not count words in messages.
    fn without_string_literals(code: &str) -> String {
        let mut out = String::new();
        let (mut in_string, mut escaped) = (false, false);
        for ch in code.chars() {
            if in_string {
                match (escaped, ch) {
                    (true, _) => escaped = false,
                    (false, '\\') => escaped = true,
                    (false, '"') => {
                        in_string = false;
                        out.push(ch);
                    }
                    _ => {}
                }
            } else {
                in_string = ch == '"';
                out.push(ch);
            }
        }
        out
    }

    /// How often `ident` occurs in `code` as a whole identifier — `retry` in
    /// `Some(retry)`, not in `password_retry`.
    fn ident_count(code: &str, ident: &str) -> usize {
        let is_ident = |c: char| c.is_alphanumeric() || c == '_';
        code.match_indices(ident)
            .filter(|(at, _)| {
                let before = code[..*at].chars().next_back();
                let after = code[at + ident.len()..].chars().next();
                !before.is_some_and(is_ident) && !after.is_some_and(is_ident)
            })
            .count()
    }

    /// How many occurrences of `call` (ending in `(`) in collapsed `code` build
    /// a value rather than match one. A pattern is `(_)`, the declaration's
    /// `(String)`, or a binding followed by `=>` or `|`.
    fn constructions_of(code: &str, call: &str) -> usize {
        code.match_indices(call)
            .filter(|(at, _)| {
                let rest = &code[at + call.len()..];
                let Some(close) = rest.find(')') else {
                    return true;
                };
                let (arg, after) = (rest[..close].trim(), rest[close + 1..].trim_start());
                let pattern = arg == "_"
                    || arg == "String"
                    || after.starts_with("=>")
                    || after.starts_with('|');
                !pattern
            })
            .count()
    }

    /// Whether `code` rebinds `ident`: `let ident`, `let mut ident`, or an
    /// assignment `ident = …` (not `==`). Only those shapes: a binding inside
    /// another pattern — a tuple or struct `let`, `if let`, `while let`, a
    /// `match` arm, `for`, a closure parameter — is not seen.
    fn rebinds(code: &str, ident: &str) -> bool {
        let is_ident = |c: char| c.is_alphanumeric() || c == '_';
        code.match_indices(ident).any(|(at, _)| {
            let before = &code[..at];
            let after = &code[at + ident.len()..];
            if before.chars().next_back().is_some_and(is_ident)
                || after.chars().next().is_some_and(is_ident)
            {
                return false;
            }
            before.ends_with("let ")
                || before.ends_with("let mut ")
                || (after.trim_start().starts_with('=') && !after.trim_start().starts_with("=="))
        })
    }

    /// Every `.rs` file under `src/`, as `(path relative to src, contents)`.
    fn all_sources() -> Vec<(String, String)> {
        fn walk(dir: &std::path::Path, root: &std::path::Path, out: &mut Vec<(String, String)>) {
            for entry in std::fs::read_dir(dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    walk(&path, root, out);
                } else if path.extension().is_some_and(|e| e == "rs") {
                    let rel = path
                        .strip_prefix(root)
                        .unwrap()
                        .to_string_lossy()
                        .replace('\\', "/");
                    out.push((rel, std::fs::read_to_string(&path).unwrap()));
                }
            }
        }
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut out = Vec::new();
        walk(&root, &root, &mut out);
        assert!(
            out.len() > 20,
            "found only {} source files under {root:?}",
            out.len()
        );
        out
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
        let source = file("ui/state/connect.rs");
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
        let source = include_str!("connect.rs");
        for (signature, pin) in [
            (
                "async fn acquire_entra(",
                "let stored = entra_stored_copy(source, acquired.refresh_rejected);",
            ),
            (
                "async fn acquire_entra(",
                "(acquired.token, source, stored)",
            ),
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
                "ui/state/connect.rs",
                "keep_entra_refresh",
                1,
                "user-answered: `RefreshKeyring::Delete`, an unticked \"stay signed in\" box \
                 once the sign-in connects. Warned by `entra_remember_hint` on the sign-in \
                 card. The silent path reaches it only for a session-kept token, whose \
                 keyring holds none",
            ),
            (
                "ui/state/connect.rs",
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
                "ui/state/connect.rs",
                "remove_saved",
                1,
                "explicit: the user removed the connection, which owns these secrets",
            ),
            (
                "ui/state/connect.rs",
                "forget_entra_refresh",
                1,
                "automatic: Entra refused the refresh token for good, so it can never \
                 work again; or explicit: an edit moved the locator it was kept under",
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
        // `shipped` stops at `mod tests`; secrets.rs exports its test module
        // (`pub(crate) mod tests`) for the mock keyring, so stop there too.
        let without_tests = |text: &str| {
            let lines: Vec<&str> = text.lines().collect();
            let end = lines
                .windows(2)
                .position(|w| w[0].trim() == "#[cfg(test)]" && w[1].contains("mod tests"))
                .unwrap_or(lines.len());
            lines[..end].join("\n")
        };
        for (rel, text) in all_sources() {
            let code = code_only(&shipped(&without_tests(&text)));
            // Every reference to a delete, not only calls: a fn pointer
            // (`let del = …delete_password_async;`), `.map(delete_password)`
            // and a `use … as` rename all still name the original, so each
            // is counted where it appears — a `use` outside any function as
            // `<module>`, which no entry classifies.
            for ident in [
                "delete_password",
                "delete_password_async",
                "delete_credential",
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

    #[test]
    fn the_prompt_says_a_password_is_stored_only_when_the_keyring_said_so() {
        // The password prompt warns that an unticked box removes a stored
        // password on `Present`, and hedges on `Unknown`. Claiming `Present`
        // for a session password would warn about a copy that may not exist;
        // claiming `Absent` for one would hide a deletion the unticked box can
        // still make.
        for (in_session, found, expected, why) in [
            (
                false,
                true,
                StoredCopy::Present,
                "the keyring supplied the refused password and still holds it",
            ),
            (
                false,
                false,
                StoredCopy::Absent,
                "the keyring was read and held nothing (or could not be read)",
            ),
            (
                true,
                true,
                StoredCopy::Unknown,
                "session memory supplied it, so the keyring was never asked",
            ),
        ] {
            assert_eq!(stored_copy(in_session, found), expected, "{why}");
        }
    }

    #[test]
    fn a_stored_password_survives_until_a_different_one_connects() {
        // The server's refusal code also comes back for a right password — an
        // expired Postgres role, a SQL Server contained user naming a missing
        // database — so a refusal no longer deletes the stored password. This
        // is the decision that does instead, once the prompt's password has
        // connected. Both polarities lose something: deleting too eagerly
        // costs a working credential, too rarely keeps re-offering a dead one.
        use StoredPassword::{Delete, Keep, Store};
        for (remember, stored, expected, why) in [
            (
                Some(true),
                Some("old"),
                Store,
                "ticked: the new password replaces the stored one",
            ),
            (
                Some(true),
                None,
                Store,
                "ticked with nothing stored: the new one is stored",
            ),
            (
                Some(true),
                Some("new"),
                Store,
                "ticked: rewriting the same value is harmless",
            ),
            (
                Some(false),
                Some("old"),
                Delete,
                "unticked: a different password connected, so the stored one is stale \
                 and the user declined to keep any",
            ),
            (
                Some(false),
                Some("new"),
                Keep,
                "unticked, but the stored password is the one that just connected",
            ),
            (
                Some(false),
                None,
                Keep,
                "unticked with nothing stored: nothing to delete",
            ),
            (
                None,
                Some("old"),
                Keep,
                "nobody was asked: the keyring is not touched",
            ),
            (None, None, Keep, "nobody was asked and nothing is stored"),
        ] {
            assert_eq!(
                stored_password_after_connect(remember, stored, "new"),
                expected,
                "{why}"
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
                        .keep_entra_refresh(&task_url, "fresh".to_string(), remember)
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
