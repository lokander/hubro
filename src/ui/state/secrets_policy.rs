//! The secret-handling policy of the connect flows, as free functions over
//! plain data: where a password, an SSH passphrase and an Entra refresh token
//! are kept, and what each answer to "remember" does to the keyring.
//!
//! Split out of `connect.rs` so the decisions can be read, and executed by a
//! test, apart from the async flows that act on them. A test here runs the
//! policy; whether a flow consults it is pinned beside the flows.

use super::*;

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
pub(super) async fn migrate_secret(old: String, new: String, validated: Option<String>) {
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
/// A declined choice is recorded as [`Remember::SessionOnly`] rather than by
/// removing the entry, because "answered no" and "never asked" are different
/// states and the difference is user-visible: only the second may offer a
/// ticked box on the next prompt. They were the same state while this was a set, which quietly
/// unticked the re-prompt for a passphrase typed into the connection form.
///
/// That form parks its answer here too. It used to stash the passphrase
/// without parking anything and store it itself once the connection opened,
/// so an answer parked by an earlier, abandoned prompt was still waiting: the
/// tunnel open redeemed it and stored the form's passphrase although its box
/// was unticked. Withdrawing on `None` closes that for a hidden box as well,
/// without recording a "no" the user never gave.
pub(super) fn park_ssh_remember(
    pending: &mut HashMap<String, Remember>,
    url: &str,
    remember: Option<Remember>,
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
pub(super) fn withdraw_ssh_remember(
    pending: &mut HashMap<String, Remember>,
    url: &str,
) -> Remember {
    pending.remove(url).unwrap_or(Remember::Keyring)
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
    pending: &mut HashMap<String, Remember>,
    url: &str,
    source: Option<PassphraseSource>,
) -> bool {
    let parked = pending.remove(url);
    parked == Some(Remember::Keyring) && source == Some(PassphraseSource::Session)
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
pub(super) fn redeemed_refresh_remembered(source: Option<RefreshSource>) -> Remember {
    match source {
        Some(RefreshSource::Keyring) => Remember::Keyring,
        Some(RefreshSource::Session) | None => Remember::SessionOnly,
    }
}

/// What the sign-in card offers after a silent attempt failed: unticked when
/// the token it tried was kept for the session only, because the user said
/// "don't remember" this run — a transient failure must not turn that into a
/// ticked box and a keyring write. Otherwise ticked, as the password prompt is.
pub(super) fn card_offers_remember(source: Option<RefreshSource>) -> Remember {
    match source {
        Some(RefreshSource::Session) => Remember::SessionOnly,
        Some(RefreshSource::Keyring) | None => Remember::Keyring,
    }
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
    remember: Remember,
) -> RefreshKeyring {
    let key = entra_secret_key(url);
    match remember {
        Remember::Keyring => {
            session.remove(&key);
            RefreshKeyring::Store(refresh)
        }
        Remember::SessionOnly => {
            session.insert(key, refresh);
            RefreshKeyring::Delete
        }
    }
}

/// The sign-outs a connection's kept Entra sign-in has had this run, keyed
/// by locator in [`AppState::entra_sign_outs`].
///
/// A sign-out is two things that are not instantaneous: a keyring deletion,
/// which is spawned I/O, and the end of every sign-in already under way, which
/// may have read the token before the click and will try to keep the rotated
/// one it gets back after it. Both are decided on this record rather than on
/// timing: `deleting` says a deletion has not finished, so the keyring copy
/// must not be redeemed ([`keyring_sign_in_redeemable`]), and `generation`
/// says whether any sign-out happened since a connect began
/// ([`kept_after_sign_in`]).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct EntraSignOut {
    generation: u64,
    deleting: u32,
}

/// Starts signing `url`'s connection out, synchronously: records the sign-out
/// and takes the kept refresh token out of session memory. Returns the
/// keyring half, which is I/O and must be followed by [`end_entra_sign_out`].
/// Always a [`RefreshKeyring::Delete`] — the verdict a "don't remember"
/// sign-in gets from [`place_entra_refresh`], minus the fresh token to keep
/// for the session, because a sign-out leaves no token in either home.
pub(super) fn begin_entra_sign_out(
    sign_outs: &mut HashMap<String, EntraSignOut>,
    session: &mut HashMap<String, String>,
    url: &str,
) -> RefreshKeyring {
    let record = sign_outs.entry(url.to_string()).or_default();
    record.generation += 1;
    record.deleting += 1;
    session.remove(&entra_secret_key(url));
    RefreshKeyring::Delete
}

/// Records that one sign-out's keyring deletion has finished, however it went.
/// A refused deletion leaves the token in the keyring, where it will sign the
/// user in again — which is what the caller then says.
pub(super) fn end_entra_sign_out(sign_outs: &mut HashMap<String, EntraSignOut>, url: &str) {
    if let Some(record) = sign_outs.get_mut(url) {
        record.deleting = record.deleting.saturating_sub(1);
    }
}

/// Which sign-out `url`'s connection is at: a connect reads it before it reads
/// the token it will redeem, and [`kept_after_sign_in`] compares.
pub(super) fn entra_sign_out_generation(
    sign_outs: &HashMap<String, EntraSignOut>,
    url: &str,
) -> u64 {
    sign_outs.get(url).map_or(0, |record| record.generation)
}

/// Whether the keyring copy of `url`'s sign-in may be redeemed: not while a
/// sign-out's deletion of it is still running, since that copy is the one the
/// user just asked to forget.
pub(super) fn keyring_sign_in_redeemable(
    sign_outs: &HashMap<String, EntraSignOut>,
    url: &str,
) -> bool {
    sign_outs.get(url).is_none_or(|record| record.deleting == 0)
}

/// Whether a sign-in may keep the refresh token it got back, now that the
/// connection is at sign-out generation `now`.
///
/// `redeemed_at` is the generation a token *redeemed from storage* was read
/// at, and `None` for a sign-in the user has just made in the browser. Only
/// the first is revoked by a sign-out since: it is the old consent, and
/// keeping its rotated successor would undo the sign-out — in the keyring when
/// the old one was remembered. A fresh sign-in is fresh consent, kept where
/// its card's answer says whatever was signed out before it — including the
/// connect's own forgetting of a refused token, which the browser sign-in
/// that follows it exists to replace. So a browser sign-in begun before a
/// "Sign out" click and finished after it is kept too, as the fresh consent
/// it is. The click is rarely reachable then: once the connect's progress
/// shows, a row whose connect came from the sign-in card offers no actions at
/// all (only a connect started from the list offers Cancel), so only the
/// moment before the progress appears leaves "Sign out" on screen.
pub(super) fn kept_after_sign_in(redeemed_at: Option<u64>, now: u64) -> bool {
    redeemed_at.is_none_or(|began| began == now)
}

/// Whether a write to the saved list leaves the entry under a locator unable
/// to keep the Entra sign-in it could keep before. Such a write signs the
/// connection out, whichever path it took: kept, the token would be a sign-in
/// no "Sign out" is offered for, and a later switch back to interactive would
/// redeem it without a card.
///
/// Decided on the auth before and after rather than on the new auth alone,
/// so an ordinary password connect — which saves through the same place —
/// pays no keyring round trip for a sign-in it never had.
pub(super) fn entra_sign_in_dropped(
    before: Option<&ServerAuth>,
    after: Option<&ServerAuth>,
) -> bool {
    before.is_some_and(ServerAuth::keeps_entra_sign_in)
        && !after.is_some_and(ServerAuth::keeps_entra_sign_in)
}

/// How executing a [`RefreshKeyring`] verdict went.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum RefreshKeyringOutcome {
    /// Stored, or nothing remembered is left.
    Applied,
    /// The keyring refused to store this token; the caller decides where it
    /// goes instead.
    StoreRefused(String),
    /// A remembered token was there and the keyring refused to delete it, so
    /// it will go on signing the user in.
    RemovalRefused,
    /// The keyring could be neither read nor written (locked, say), so
    /// whether a remembered token is still there is unknown.
    Unreachable,
}

/// Executes a [`RefreshKeyring`] verdict against the keyring entry for
/// `url`'s refresh token. The one path both a sign-in's answer
/// ([`AppState::keep_entra_refresh`]) and a sign-out
/// ([`AppState::sign_out_entra`]) take to the keyring.
///
/// A delete always runs; the read before it only decides what a failure
/// means. A token that was there and stayed is `RemovalRefused`. A keyring
/// that answers neither is `Unreachable` — unless the keyring is switched off
/// (`HUBRO_DISABLE_KEYRING`), where nothing can have been remembered.
pub(super) async fn apply_refresh_keyring(
    url: &str,
    verdict: RefreshKeyring,
) -> RefreshKeyringOutcome {
    let key = entra_secret_key(url);
    match verdict {
        RefreshKeyring::Store(refresh) => {
            match crate::secrets::store_password_async(key, refresh.clone()).await {
                Ok(()) => RefreshKeyringOutcome::Applied,
                Err(_) => RefreshKeyringOutcome::StoreRefused(refresh),
            }
        }
        RefreshKeyring::Delete => {
            let held = crate::secrets::get_password_async(key.clone()).await;
            let deleted = crate::secrets::delete_password_async(key).await;
            match (held, deleted) {
                (Ok(Some(_)), Err(_)) => RefreshKeyringOutcome::RemovalRefused,
                (Err(_), Err(_)) if !crate::secrets::keyring_disabled() => {
                    RefreshKeyringOutcome::Unreachable
                }
                _ => RefreshKeyringOutcome::Applied,
            }
        }
    }
}

/// What the connections screen says once a sign-out has gone through. `open`
/// is whether a tab on the connection is open, which the sign-out does not
/// touch (see [`AppState::sign_out_entra`]).
pub(super) fn sign_out_notice(name: &str, open: bool) -> String {
    if open {
        format!(
            "Signed out of {name}. Its open tab stays connected, and keeps its \
             sign-in until you close it; the next connect asks you to sign in again."
        )
    } else {
        format!("Signed out of {name}. The next connect asks you to sign in again.")
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
/// The answer is *removed* rather than set to [`Remember::SessionOnly`],
/// because those are different states here too: a recreated connection has
/// never been asked, so its first prompt must come up ticked like any other
/// ([`withdraw_ssh_remember`] is what tells them apart).
pub(super) fn forget_connection_secrets(
    session: &mut HashMap<String, String>,
    pending: &mut HashMap<String, Remember>,
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

/// The notice for a connect whose secret — `what`, such as "the password" —
/// the keyring refused to store. The secret is kept for the session instead,
/// so the connect itself stands. One wording for every secret, so the notices
/// cannot drift apart as they once had.
pub(super) fn keyring_store_failed(what: &str) -> String {
    format!(
        "connected, but {what} could not be stored in the system keyring — it is \
         remembered for this session only"
    )
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
    remember: Option<Remember>,
    stored: Option<&str>,
    connected: &str,
) -> StoredPassword {
    match remember {
        Some(Remember::Keyring) => StoredPassword::Store,
        Some(Remember::SessionOnly) if stored.is_some_and(|stored| stored != connected) => {
            StoredPassword::Delete
        }
        Some(Remember::SessionOnly) | None => StoredPassword::Keep,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_refused_keyring_store_is_worded_alike() {
        assert_eq!(
            keyring_store_failed("the password"),
            "connected, but the password could not be stored in the system keyring — it \
             is remembered for this session only"
        );
        // Every such notice is built here. No shipped string literal spells
        // the wording out itself — read as the compiler joins it, since
        // rustfmt wraps a long message across lines and a search of the raw
        // text misses it wherever the wrap fell — and each kind of secret has
        // its call.
        let mut calls = Vec::new();
        let mut every_call = 0;
        for (file, source) in super::super::source_reading::all_sources() {
            let code = crate::source_scan::code_outside_test_modules(&source);
            if file != "ui/state/secrets_policy.rs" {
                for literal in crate::source_scan::string_literals(&code) {
                    assert!(
                        !literal.contains("could not be stored in the system keyring"),
                        "src/{file} words a refused keyring store itself: {literal}"
                    );
                }
            }
            // Every call, whatever its argument: one built from a variable
            // would escape the literal-argument count below.
            every_call += code
                .match_indices("keyring_store_failed(")
                .filter(|(at, _)| {
                    let before = &code[..*at];
                    !before.ends_with("fn ")
                        && !before
                            .chars()
                            .next_back()
                            .is_some_and(|c| c.is_alphanumeric() || c == '_')
                })
                .count();
            for (at, _) in code.match_indices("keyring_store_failed(\"") {
                let what = &code[at + "keyring_store_failed(\"".len()..];
                calls.push(what[..what.find('"').unwrap()].to_string());
            }
        }
        assert_eq!(
            every_call, 3,
            "a refused-store notice is raised outside the three gates"
        );
        calls.sort();
        assert_eq!(
            calls,
            ["the SSH key passphrase", "the password", "the sign-in"]
        );

        // And each notice is raised exactly when its own store failed: the
        // store call, the binding its result is read through, the gate and
        // the notice, as written — comments out and whitespace removed, so a
        // rustfmt wrap is no failure.
        let flows: String =
            crate::source_scan::without_comments(&super::super::source_reading::connect_code())
                .split_whitespace()
                .collect();
        for gate in [
            "letstore=crate::secrets::store_password_async(url.clone(),password.clone()).await;\
             ifstore.is_err(){self.connect_error.set(Some(keyring_store_failed(\"thepassword\")));",
            "ifcrate::secrets::store_password_async(key,passphrase).await.is_err(){\
             self.connect_error.set(Some(keyring_store_failed(\"theSSHkeypassphrase\")));",
            "matchcrate::secrets::store_password_async(key,refresh.clone()).await{\
             Ok(())=>RefreshKeyringOutcome::Applied,Err(_)=>RefreshKeyringOutcome::StoreRefused(refresh),}",
            "matchapply_refresh_keyring(url,placed).await{",
            "RefreshKeyringOutcome::StoreRefused(refresh)=>{self.session_passwords.write().insert(entra_secret_key(url),refresh);\
             self.connect_error.set(Some(keyring_store_failed(\"thesign-in\")));",
        ] {
            assert!(
                flows.contains(gate),
                "a refused-store notice is raised on something other than the \
                 store failing: `{gate}` is gone"
            );
        }
    }

    #[test]
    fn a_silent_sign_in_keeps_its_token_where_the_user_put_it() {
        // Only a remembered token is in the keyring, so its rotation stays
        // there; one kept for the session stays for the session. No source
        // (managed identity, which returns no refresh token) is never a
        // reason to start remembering.
        assert_eq!(
            redeemed_refresh_remembered(Some(RefreshSource::Keyring)),
            Remember::Keyring
        );
        assert_eq!(
            redeemed_refresh_remembered(Some(RefreshSource::Session)),
            Remember::SessionOnly
        );
        assert_eq!(redeemed_refresh_remembered(None), Remember::SessionOnly);

        // The card after a failed silent attempt: a "don't remember" given
        // this run stands, anything else is offered ticked.
        assert_eq!(
            card_offers_remember(Some(RefreshSource::Session)),
            Remember::SessionOnly
        );
        assert_eq!(
            card_offers_remember(Some(RefreshSource::Keyring)),
            Remember::Keyring
        );
        assert_eq!(card_offers_remember(None), Remember::Keyring);
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
            place_entra_refresh(&mut session, url, "fresh".to_string(), Remember::Keyring),
            RefreshKeyring::Store("fresh".to_string())
        );
        assert!(!session.contains_key(&key));

        // Declined: for the session only, and any remembered one is deleted,
        // since keeping it would sign the user in at the next start anyway.
        assert_eq!(
            place_entra_refresh(
                &mut session,
                url,
                "fresh".to_string(),
                Remember::SessionOnly
            ),
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
        park_ssh_remember(&mut pending, url, Some(Remember::Keyring));
        assert!(redeem_ssh_remember(
            &mut pending,
            url,
            Some(PassphraseSource::Session)
        ));

        // Unticked: never stored, however the passphrase was accepted.
        park_ssh_remember(&mut pending, url, Some(Remember::SessionOnly));
        assert!(!redeem_ssh_remember(
            &mut pending,
            url,
            Some(PassphraseSource::Session)
        ));

        // An earlier tick must not survive a later untick — the intent is
        // keyed by locator and outlives the attempt that made it.
        park_ssh_remember(&mut pending, url, Some(Remember::Keyring));
        park_ssh_remember(&mut pending, url, Some(Remember::SessionOnly));
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
        park_ssh_remember(&mut pending, url, Some(Remember::Keyring));
        park_ssh_remember(&mut pending, url, None);
        assert!(
            !redeem_ssh_remember(&mut pending, url, Some(PassphraseSource::Session)),
            "an answer left by an earlier attempt survived an unasked one"
        );
        park_ssh_remember(&mut pending, url, Some(Remember::SessionOnly));
        park_ssh_remember(&mut pending, url, None);
        assert_eq!(
            withdraw_ssh_remember(&mut pending, url),
            Remember::Keyring,
            "an unasked answer was recorded as a no"
        );
    }

    #[test]
    fn a_redeemed_choice_is_consumed_and_never_rewrites_the_keyring() {
        let url = "postgres://u@h:5432/db";

        // One tick, one write: the next acceptance must not store again.
        let mut pending = HashMap::new();
        park_ssh_remember(&mut pending, url, Some(Remember::Keyring));
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
        park_ssh_remember(&mut pending, url, Some(Remember::Keyring));
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
        park_ssh_remember(&mut pending, url, Some(Remember::Keyring));
        assert!(!redeem_ssh_remember(&mut pending, url, None));
        assert!(pending.is_empty());

        // Other locators are untouched throughout.
        let mut pending = HashMap::new();
        park_ssh_remember(&mut pending, url, Some(Remember::Keyring));
        park_ssh_remember(
            &mut pending,
            "postgres://u@other:5432/db",
            Some(Remember::Keyring),
        );
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
    fn a_removed_connection_leaves_nothing_behind_in_memory() {
        // `remove_saved` deletes the three keyring entries; their
        // session-lived counterparts stayed. The parked intent is again the
        // sharp edge — recreating the same URL in the same session would
        // redeem it and write back the passphrase the deletion just removed.
        const URL: &str = "postgres://u@h:5432/db";
        let mut session = session_for(URL);
        let mut pending = HashMap::new();
        park_ssh_remember(&mut pending, URL, Some(Remember::Keyring));

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
        park_ssh_remember(&mut pending, URL, Some(Remember::Keyring));
        forget_connection_secrets(&mut session_for(URL), &mut pending, URL);
        assert_eq!(
            withdraw_ssh_remember(&mut pending, URL),
            Remember::Keyring,
            "the deleted connection left an answer behind, so recreating it \
             offers an unticked box the user never cleared"
        );
        assert!(pending.is_empty(), "the deletion left an entry behind");

        let mut session = session_for(URL);
        let mut pending = HashMap::new();
        park_ssh_remember(&mut pending, URL, Some(Remember::Keyring));
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
        park_ssh_remember(&mut pending, OTHER, Some(Remember::Keyring));
        forget_connection_secrets(&mut session, &mut pending, URL);
        assert_eq!(session, session_for(OTHER));
        assert!(pending.contains_key(OTHER));
    }

    #[test]
    fn a_re_prompt_offers_the_choice_the_user_already_made() {
        // `remember` was `use_signal(|| true)`, so every re-prompt
        // arrived freshly ticked: unticking the box and then mistyping the
        // passphrase silently restored the decision to store it.
        const URL: &str = "postgres://u@h:5432/db";

        // Unticked, typed, rejected: the re-prompt still offers unticked.
        let mut pending = HashMap::new();
        park_ssh_remember(&mut pending, URL, Some(Remember::SessionOnly));
        assert_eq!(
            withdraw_ssh_remember(&mut pending, URL),
            Remember::SessionOnly,
            "a mistyped passphrase re-ticked a box the user had cleared"
        );

        // Ticked, typed, rejected: still ticked, and the intent is withdrawn
        // either way — it was made about the passphrase that just failed.
        let mut pending = HashMap::new();
        park_ssh_remember(&mut pending, URL, Some(Remember::Keyring));
        assert_eq!(withdraw_ssh_remember(&mut pending, URL), Remember::Keyring);
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
        assert_eq!(
            withdraw_ssh_remember(&mut pending, URL),
            Remember::Keyring,
            "a first prompt for this connection came up unticked"
        );
        park_ssh_remember(
            &mut pending,
            "postgres://u@other:5432/db",
            Some(Remember::SessionOnly),
        );
        assert_eq!(
            withdraw_ssh_remember(&mut pending, URL),
            Remember::Keyring,
            "another connection's declined choice decided this prompt"
        );

        // Another connection's parked choice is not withdrawn with it.
        let mut pending = HashMap::new();
        park_ssh_remember(
            &mut pending,
            "postgres://u@other:5432/db",
            Some(Remember::Keyring),
        );
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
                Some(Remember::Keyring),
                Some("old"),
                Store,
                "ticked: the new password replaces the stored one",
            ),
            (
                Some(Remember::Keyring),
                None,
                Store,
                "ticked with nothing stored: the new one is stored",
            ),
            (
                Some(Remember::Keyring),
                Some("new"),
                Store,
                "ticked: rewriting the same value is harmless",
            ),
            (
                Some(Remember::SessionOnly),
                Some("old"),
                Delete,
                "unticked: a different password connected, so the stored one is stale \
                 and the user declined to keep any",
            ),
            (
                Some(Remember::SessionOnly),
                Some("new"),
                Keep,
                "unticked, but the stored password is the one that just connected",
            ),
            (
                Some(Remember::SessionOnly),
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

    #[test]
    fn only_a_connection_that_can_keep_a_sign_in_offers_to_sign_out() {
        // The gate on the wire below, executed: an interactive sign-in is the
        // only one that hands back a refresh token. Offered to a managed
        // identity or a password connection, "Sign out" would claim to undo
        // something that never happened.
        for (auth, keeps) in [
            (ServerAuth::Entra(EntraAuth::interactive_default()), true),
            (
                ServerAuth::Entra(EntraAuth::Interactive {
                    tenant: "contoso.onmicrosoft.com".into(),
                    client_id: Some("app".into()),
                }),
                true,
            ),
            (
                ServerAuth::Entra(EntraAuth::ManagedIdentity { client_id: None }),
                false,
            ),
            (ServerAuth::Password, false),
        ] {
            assert_eq!(auth.keeps_entra_sign_in(), keeps, "{auth:?}");
        }
        assert_eq!(
            sign_out_notice("prod", false),
            "Signed out of prod. The next connect asks you to sign in again."
        );
        assert!(
            sign_out_notice("prod", true).contains("open tab stays connected"),
            "a sign-out that leaves a tab connected says so"
        );
    }

    #[test]
    fn a_sign_out_record_decides_redeeming_and_keeping() {
        // The record's policy, executed. While a deletion runs the keyring
        // copy is not redeemable; once it has run, it is again (a refused
        // deletion leaves a token that will sign the user in, and says so).
        // A connect keeps its token only when no sign-out came since it
        // began — however many there were.
        let mut sign_outs = HashMap::new();
        let mut session = HashMap::from([(entra_secret_key("a"), "t".to_string())]);
        assert!(keyring_sign_in_redeemable(&sign_outs, "a"));
        let began = entra_sign_out_generation(&sign_outs, "a");
        assert_eq!(
            begin_entra_sign_out(&mut sign_outs, &mut session, "a"),
            RefreshKeyring::Delete
        );
        assert!(session.is_empty(), "the session copy outlives the sign-out");
        assert!(!keyring_sign_in_redeemable(&sign_outs, "a"));
        assert!(
            keyring_sign_in_redeemable(&sign_outs, "b"),
            "one locator only"
        );
        let _ = begin_entra_sign_out(&mut sign_outs, &mut session, "a");
        end_entra_sign_out(&mut sign_outs, "a");
        assert!(
            !keyring_sign_in_redeemable(&sign_outs, "a"),
            "a second sign-out's deletion is still running"
        );
        end_entra_sign_out(&mut sign_outs, "a");
        assert!(keyring_sign_in_redeemable(&sign_outs, "a"));
        let now = entra_sign_out_generation(&sign_outs, "a");
        assert!(
            !kept_after_sign_in(Some(began), now),
            "a token redeemed before a sign-out outlives it"
        );
        assert!(kept_after_sign_in(Some(now), now));
        assert!(
            kept_after_sign_in(None, now),
            "a sign-in the user just made in the browser is thrown away because \
             something was signed out before it"
        );
        assert!(kept_after_sign_in(
            Some(entra_sign_out_generation(&sign_outs, "b")),
            entra_sign_out_generation(&sign_outs, "b")
        ));
    }

    #[test]
    fn a_save_signs_out_exactly_when_the_entry_can_no_longer_keep_a_sign_in() {
        // The add-or-edit branch's predicate, executed over (before, after).
        let interactive = ServerAuth::Entra(EntraAuth::interactive_default());
        let managed = ServerAuth::Entra(EntraAuth::ManagedIdentity { client_id: None });
        let password = ServerAuth::Password;
        for (before, after, signs_out) in [
            (Some(&interactive), Some(&password), true),
            (Some(&interactive), Some(&managed), true),
            (Some(&interactive), None, true),
            (Some(&interactive), Some(&interactive), false),
            // An ordinary password connect saves through the same place and
            // must not pay a keyring round trip for a sign-in it never had.
            (Some(&password), Some(&password), false),
            (Some(&managed), Some(&password), false),
            (None, Some(&password), false),
            (None, Some(&interactive), false),
        ] {
            assert_eq!(
                entra_sign_in_dropped(before, after),
                signs_out,
                "{before:?} → {after:?}"
            );
        }
    }
}
