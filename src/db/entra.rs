//! Renewing a Microsoft Entra access token for the connections a pool opens
//! after the connect.
//!
//! An Entra login is an access token that expires (about an hour). The connect
//! flow acquires one and logs in with it, but a pool goes on opening
//! connections for as long as it is open: every script run (a run's connection
//! is closed when it ends), and, below hubro, a replacement for an idle
//! connection the pool dropped. Each of those logs in afresh, and with the
//! connect's token they failed once it had expired — while the grid went on
//! working on connections that had logged in earlier.
//!
//! [`EntraTokens`] is what such a pool asks for the token to log in with. It
//! hands back the current one while it is good for at least [`RENEW_MARGIN`]
//! more, and otherwise renews it **silently**, through the `renew` function the
//! connect flow gave it — a kept refresh token redeemed, or a managed identity
//! asked — or says the user has to reconnect. Nothing here can open a browser:
//! the renewal it is given has no interactive branch (see
//! `azure::renew_silently`). Nothing here writes a token anywhere either: a
//! refresh token the renewal hands back replaces the one held here, in memory,
//! for as long as the pool lives, and the keyring and session memory keep the
//! one the connect placed there by the user's answer.
//!
//! Which way to go is [`token_step`], a free function over plain data.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use super::error::DbError;

/// How long before its expiry a token is renewed: long enough that a token
/// handed to a login does not expire during it, or between a pool's renewal
/// checks ([`super::postgres`] checks every minute).
pub const RENEW_MARGIN: Duration = Duration::from_secs(5 * 60);

/// How long a renewal that failed for a reason that may pass (the network,
/// Entra being unavailable) is not tried again. Without it every connection the
/// pool opens would retry at once — a retry storm against the identity
/// provider while it is down.
pub const RETRY_AFTER: Duration = Duration::from_secs(30);

/// A token's value — an access token or a refresh token. Its `Debug` and
/// `Display` print `<redacted>`, so a token formatted into a log line or an
/// error by mistake gives nothing away; the value itself is reached only
/// through [`Self::expose`], which the logins call to hand it to the driver.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(String);

impl Secret {
    pub fn new(value: impl Into<String>) -> Self {
        Secret(value.into())
    }

    /// The value, for the one thing that may read it: the login.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("<redacted>")
    }
}

impl std::fmt::Display for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("<redacted>")
    }
}

/// An access token and when it expires.
#[derive(Clone)]
pub struct Token {
    pub secret: Secret,
    pub expires_at: SystemTime,
}

/// What a renewal returned: the new access token, and the refresh token to
/// redeem next time when the identity provider rotated it.
pub struct Renewed {
    pub token: Token,
    pub refresh: Option<Secret>,
}

/// Why a renewal failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RenewError {
    /// The refresh token was refused for good (revoked, expired, the user's
    /// password changed): it is dropped, and only a new sign-in helps.
    RefreshRejected(String),
    /// Anything else — it may work in a while ([`RETRY_AFTER`]).
    Failed(String),
}

pub type RenewFuture = Pin<Box<dyn Future<Output = Result<Renewed, RenewError>> + Send>>;

/// The silent renewal: given the refresh token held (if any), a new access
/// token. Built by the connect flow for the connection's auth mode.
pub type Renew = Arc<dyn Fn(Option<Secret>) -> RenewFuture + Send + Sync>;

/// The wall clock — injected so a test can move past an expiry.
pub type Clock = Arc<dyn Fn() -> SystemTime + Send + Sync>;

/// What to do for a login that needs a token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TokenStep {
    /// Log in with the token held.
    Use,
    /// Renew it first.
    Renew,
    /// It has expired and cannot be renewed now: fail the login, saying why.
    Refuse,
}

/// Which way to go, given the time, the held token's expiry, whether a silent
/// renewal is possible at all ([`may_renew`]) and when the last one failed.
///
/// A token still good for more than [`RENEW_MARGIN`] is used. Inside the margin
/// it is renewed — unless renewal is impossible, or failed less than
/// [`RETRY_AFTER`] ago, in which case the held token is used while it lasts and
/// refused once it has expired.
pub fn token_step(
    now: SystemTime,
    expires_at: SystemTime,
    can_renew: bool,
    last_failure: Option<SystemTime>,
) -> TokenStep {
    if !renewal_due(now, expires_at) {
        return TokenStep::Use;
    }
    let backing_off = last_failure.is_some_and(|failed| {
        now.duration_since(failed)
            .map(|since| since < RETRY_AFTER)
            // A clock that went backwards: the failure is in the future, so
            // count it as recent rather than retry in a loop.
            .unwrap_or(true)
    });
    if can_renew && !backing_off {
        TokenStep::Renew
    } else if now < expires_at {
        TokenStep::Use
    } else {
        TokenStep::Refuse
    }
}

/// Whether a token expiring at `expires_at` is within [`RENEW_MARGIN`] of it.
pub fn renewal_due(now: SystemTime, expires_at: SystemTime) -> bool {
    now + RENEW_MARGIN >= expires_at
}

/// Whether a token can be renewed without the user: a managed identity always
/// can, an interactive sign-in only by redeeming a refresh token the connect
/// kept. The same rule as `EntraAuth::can_acquire_silently`, over plain data.
pub fn may_renew(managed_identity: bool, holds_refresh: bool) -> bool {
    managed_identity || holds_refresh
}

struct State {
    token: Token,
    refresh: Option<Secret>,
    last_failure: Option<SystemTime>,
    /// Why the last renewal failed, for the refusal.
    why: Option<String>,
}

/// The token a pool logs its new connections in with — see the module docs.
///
/// One renewal at a time: the state sits behind an async mutex held across the
/// renewal, so connections opened together wait for one renewal and then share
/// its token rather than each starting their own.
pub struct EntraTokens {
    state: tokio::sync::Mutex<State>,
    managed_identity: bool,
    renew: Renew,
    now: Clock,
}

impl EntraTokens {
    /// `token` is the one the connect logged in with; `refresh` the refresh
    /// token it got back, if any — held here in memory only.
    pub fn new(
        token: Token,
        refresh: Option<Secret>,
        managed_identity: bool,
        renew: Renew,
        now: Clock,
    ) -> Self {
        EntraTokens {
            state: tokio::sync::Mutex::new(State {
                token,
                refresh,
                last_failure: None,
                why: None,
            }),
            managed_identity,
            renew,
            now,
        }
    }

    /// [`Self::new`] on the system clock.
    pub fn on_system_clock(
        token: Token,
        refresh: Option<Secret>,
        managed_identity: bool,
        renew: Renew,
    ) -> Self {
        Self::new(
            token,
            refresh,
            managed_identity,
            renew,
            Arc::new(SystemTime::now),
        )
    }

    /// The token to log a new connection in with, renewed first when
    /// [`token_step`] says so.
    ///
    /// The clock is read again once a renewal returns: one can take as long
    /// as its HTTP timeout, and a failure stamped with the time it *started*
    /// would already be [`RETRY_AFTER`] old when it is recorded — measured,
    /// five callers queued behind a renewal that hung made five renewals in
    /// series — and a token that expired meanwhile would still be handed out.
    pub async fn current(&self) -> Result<Secret, DbError> {
        let mut state = self.state.lock().await;
        let now = (self.now)();
        let can_renew = may_renew(self.managed_identity, state.refresh.is_some());
        match token_step(now, state.token.expires_at, can_renew, state.last_failure) {
            TokenStep::Use => return Ok(state.token.secret.clone()),
            TokenStep::Refuse => return Err(sign_in_again(state.why.as_deref())),
            TokenStep::Renew => {}
        }
        let renewed = (self.renew)(state.refresh.clone()).await;
        let now = (self.now)();
        match renewed {
            Ok(Renewed { token, refresh }) => {
                state.token = token;
                if refresh.is_some() {
                    state.refresh = refresh;
                }
                // A renewal can hand back a token that is itself inside the
                // margin — a managed identity's endpoint returns its cached
                // token until shortly before expiry. Counted as a failure for
                // the backoff, or every login and every tick would renew
                // again at once.
                if renewal_due(now, state.token.expires_at) {
                    state.last_failure = Some(now);
                    state.why = Some("the renewed token expires within minutes".to_string());
                } else {
                    state.last_failure = None;
                    state.why = None;
                }
            }
            Err(error) => {
                let why = match error {
                    RenewError::RefreshRejected(why) => {
                        state.refresh = None;
                        why
                    }
                    RenewError::Failed(why) => why,
                };
                state.last_failure = Some(now);
                state.why = Some(why);
            }
        }
        if now < state.token.expires_at {
            Ok(state.token.secret.clone())
        } else {
            Err(sign_in_again(state.why.as_deref()))
        }
    }
}

/// The refusal a login gets once the token has expired and cannot be renewed.
fn sign_in_again(why: Option<&str>) -> DbError {
    let why = why.map(|why| format!(" ({why})")).unwrap_or_default();
    DbError::Connect(format!(
        "the Microsoft Entra sign-in for this connection has expired and could not \
         be renewed without you{why} — reconnect to sign in again"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

    const T0: Duration = Duration::from_secs(1_000_000);

    fn at(secs: u64) -> SystemTime {
        SystemTime::UNIX_EPOCH + T0 + Duration::from_secs(secs)
    }

    #[test]
    fn a_token_is_used_until_the_margin_then_renewed() {
        let expires = at(3600);
        assert_eq!(token_step(at(0), expires, true, None), TokenStep::Use);
        let inside = at(3600) - RENEW_MARGIN;
        assert_eq!(token_step(inside, expires, true, None), TokenStep::Renew);
        assert_eq!(token_step(at(3599), expires, true, None), TokenStep::Renew);
        assert_eq!(token_step(at(4000), expires, true, None), TokenStep::Renew);
    }

    #[test]
    fn a_token_that_cannot_be_renewed_is_used_until_it_expires() {
        let expires = at(3600);
        assert_eq!(token_step(at(3500), expires, false, None), TokenStep::Use);
        assert_eq!(
            token_step(at(3600), expires, false, None),
            TokenStep::Refuse
        );
        assert_eq!(
            token_step(at(9999), expires, false, None),
            TokenStep::Refuse
        );
    }

    #[test]
    fn a_failed_renewal_is_not_retried_at_once() {
        let expires = at(3600);
        // Failed inside the margin: the token is still used while it lasts.
        let failed = Some(at(3590));
        assert_eq!(token_step(at(3595), expires, true, failed), TokenStep::Use);
        // Failed as it expired: refused, without another try, until
        // `RETRY_AFTER` has passed. Stated against the constant, so the test
        // pins the rule rather than one value of it.
        let failed = Some(expires);
        let just_before = expires + RETRY_AFTER - Duration::from_secs(1);
        assert_eq!(
            token_step(just_before, expires, true, failed),
            TokenStep::Refuse
        );
        let later = expires + RETRY_AFTER;
        assert_eq!(token_step(later, expires, true, failed), TokenStep::Renew);
        // A clock that went back before the failure does not retry: it uses
        // the token while it lasts and refuses it after.
        let failed = Some(at(3590));
        assert_eq!(token_step(at(3000), at(3100), true, failed), TokenStep::Use);
        assert_eq!(
            token_step(at(3200), at(3100), true, failed),
            TokenStep::Refuse
        );
    }

    #[test]
    fn only_a_managed_identity_or_a_kept_refresh_token_renews() {
        assert!(may_renew(true, false));
        assert!(may_renew(false, true));
        assert!(!may_renew(false, false));
    }

    /// A clock a test moves, and a renewal that counts its calls, records the
    /// refresh token it was handed, and answers with `answer`.
    struct Rig {
        clock: Arc<AtomicU64>,
        calls: Arc<AtomicUsize>,
        handed: Arc<std::sync::Mutex<Vec<Option<String>>>>,
    }

    fn rig(
        refresh: Option<&str>,
        managed: bool,
        answer: impl Fn(usize) -> Result<Renewed, RenewError> + Send + Sync + 'static,
    ) -> (EntraTokens, Rig) {
        let clock = Arc::new(AtomicU64::new(0));
        let calls = Arc::new(AtomicUsize::new(0));
        let handed = Arc::new(std::sync::Mutex::new(Vec::new()));
        let answer = Arc::new(answer);
        let renew: Renew = {
            let (calls, handed) = (Arc::clone(&calls), Arc::clone(&handed));
            Arc::new(move |refresh| {
                let n = calls.fetch_add(1, Ordering::SeqCst);
                handed
                    .lock()
                    .unwrap()
                    .push(refresh.map(|r| r.expose().to_string()));
                let answer = Arc::clone(&answer);
                Box::pin(async move {
                    // Long enough for concurrent callers to pile up behind it.
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    answer(n)
                })
            })
        };
        let now: Clock = {
            let clock = Arc::clone(&clock);
            Arc::new(move || at(clock.load(Ordering::SeqCst)))
        };
        let tokens = EntraTokens::new(
            Token {
                secret: Secret::new("first"),
                expires_at: at(3600),
            },
            refresh.map(Secret::new),
            managed,
            renew,
            now,
        );
        (
            tokens,
            Rig {
                clock,
                calls,
                handed,
            },
        )
    }

    fn renewed(secret: &str, refresh: Option<&str>) -> Result<Renewed, RenewError> {
        Ok(Renewed {
            token: Token {
                secret: Secret::new(secret),
                expires_at: at(7200),
            },
            refresh: refresh.map(Secret::new),
        })
    }

    #[tokio::test]
    async fn an_expiring_token_is_renewed_once_for_connections_opened_together() {
        let (tokens, rig) = rig(Some("r1"), false, |_| renewed("second", Some("r2")));
        assert_eq!(tokens.current().await.unwrap().expose(), "first");
        assert_eq!(rig.calls.load(Ordering::SeqCst), 0);

        rig.clock.store(3500, Ordering::SeqCst);
        let tokens = Arc::new(tokens);
        let logins: Vec<_> = (0..8)
            .map(|_| {
                let tokens = Arc::clone(&tokens);
                tokio::spawn(async move { tokens.current().await })
            })
            .collect();
        for login in logins {
            assert_eq!(login.await.unwrap().unwrap().expose(), "second");
        }
        assert_eq!(rig.calls.load(Ordering::SeqCst), 1, "one renewal, shared");

        // The rotated refresh token is the one redeemed next time.
        rig.clock.store(7100, Ordering::SeqCst);
        tokens.current().await.unwrap();
        assert_eq!(
            *rig.handed.lock().unwrap(),
            vec![Some("r1".to_string()), Some("r2".to_string())]
        );
    }

    #[tokio::test]
    async fn without_a_refresh_token_an_interactive_sign_in_is_refused_once_expired() {
        let (tokens, rig) = rig(None, false, |_| renewed("never", None));
        rig.clock.store(3500, Ordering::SeqCst);
        assert_eq!(tokens.current().await.unwrap().expose(), "first");
        rig.clock.store(3600, Ordering::SeqCst);
        let refused = tokens.current().await.unwrap_err().to_string();
        assert!(refused.contains("reconnect to sign in again"), "{refused}");
        assert_eq!(rig.calls.load(Ordering::SeqCst), 0, "nothing to renew with");
    }

    #[tokio::test]
    async fn a_managed_identity_renews_without_a_refresh_token() {
        let (tokens, rig) = rig(None, true, |_| renewed("second", None));
        rig.clock.store(3600, Ordering::SeqCst);
        assert_eq!(tokens.current().await.unwrap().expose(), "second");
        assert_eq!(*rig.handed.lock().unwrap(), vec![None]);
    }

    #[tokio::test]
    async fn a_refused_refresh_token_is_dropped_and_not_redeemed_again() {
        let (tokens, rig) = rig(Some("r1"), false, |_| {
            Err(RenewError::RefreshRejected("invalid_grant".into()))
        });
        rig.clock.store(3500, Ordering::SeqCst);
        // Still good: used, and the failure is remembered.
        assert_eq!(tokens.current().await.unwrap().expose(), "first");
        rig.clock.store(4000, Ordering::SeqCst);
        let refused = tokens.current().await.unwrap_err().to_string();
        assert!(refused.contains("invalid_grant"), "{refused}");
        assert_eq!(rig.calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn a_renewal_that_failed_waits_before_it_is_tried_again() {
        let (tokens, rig) = rig(Some("r1"), false, |n| {
            if n == 0 {
                Err(RenewError::Failed("network down".into()))
            } else {
                renewed("second", None)
            }
        });
        rig.clock.store(3600, Ordering::SeqCst);
        assert!(tokens.current().await.is_err());
        assert!(tokens.current().await.is_err());
        assert_eq!(rig.calls.load(Ordering::SeqCst), 1, "no retry storm");
        rig.clock
            .store(3600 + RETRY_AFTER.as_secs(), Ordering::SeqCst);
        assert_eq!(tokens.current().await.unwrap().expose(), "second");
    }

    /// A provider whose renewal moves the injected clock on by `takes` while
    /// it runs — a renewal waiting out its HTTP timeout — and then answers
    /// with `answer`, counting its calls.
    fn slow_rig(
        start: u64,
        takes: u64,
        answer: impl Fn() -> Result<Renewed, RenewError> + Send + Sync + 'static,
    ) -> (Arc<EntraTokens>, Arc<AtomicUsize>) {
        let clock = Arc::new(AtomicU64::new(start));
        let calls = Arc::new(AtomicUsize::new(0));
        let answer = Arc::new(answer);
        let renew: Renew = {
            let (clock, calls) = (Arc::clone(&clock), Arc::clone(&calls));
            Arc::new(move |_refresh| {
                calls.fetch_add(1, Ordering::SeqCst);
                let (clock, answer) = (Arc::clone(&clock), Arc::clone(&answer));
                Box::pin(async move {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                    clock.fetch_add(takes, Ordering::SeqCst);
                    answer()
                })
            })
        };
        let now: Clock = {
            let clock = Arc::clone(&clock);
            Arc::new(move || at(clock.load(Ordering::SeqCst)))
        };
        let tokens = EntraTokens::new(
            Token {
                secret: Secret::new("first"),
                expires_at: at(3600),
            },
            Some(Secret::new("r1")),
            false,
            renew,
            now,
        );
        (Arc::new(tokens), calls)
    }

    #[tokio::test]
    async fn a_renewal_that_hangs_is_not_repeated_by_the_callers_queued_behind_it() {
        // Takes longer than `RETRY_AFTER`: stamped with the time it started,
        // its failure was already stale, and each queued caller renewed again.
        let takes = RETRY_AFTER.as_secs() + 10;
        let (tokens, calls) = slow_rig(3600, takes, || Err(RenewError::Failed("timed out".into())));
        let logins: Vec<_> = (0..5)
            .map(|_| {
                let tokens = Arc::clone(&tokens);
                tokio::spawn(async move { tokens.current().await })
            })
            .collect();
        for login in logins {
            assert!(login.await.unwrap().is_err());
        }
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "one renewal, not one per caller"
        );
    }

    #[tokio::test]
    async fn a_token_that_expires_while_its_renewal_runs_is_not_handed_out() {
        // Inside the margin but still good when the renewal starts; expired by
        // the time it fails.
        let (tokens, _) = slow_rig(3550, 100, || Err(RenewError::Failed("timed out".into())));
        let refused = tokens.current().await.unwrap_err().to_string();
        assert!(refused.contains("reconnect to sign in again"), "{refused}");
    }

    #[tokio::test]
    async fn a_renewed_token_still_inside_the_margin_backs_off_like_a_failure() {
        // A managed identity's endpoint can hand back its cached token, a
        // minute from expiry: renewing again on every login would hammer it.
        let (tokens, calls) = slow_rig(3500, 0, || {
            Ok(Renewed {
                token: Token {
                    secret: Secret::new("cached"),
                    expires_at: at(3560),
                },
                refresh: None,
            })
        });
        for _ in 0..5 {
            assert_eq!(tokens.current().await.unwrap().expose(), "cached");
        }
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn a_secret_never_formats_its_value() {
        let secret = Secret::new("eyJ0eXAi.secret");
        assert_eq!(format!("{secret}"), "<redacted>");
        assert_eq!(format!("{secret:?}"), "<redacted>");
        let token = Token {
            secret,
            expires_at: at(0),
        };
        assert!(!format!("{:?}", token.secret).contains("eyJ0eXAi"));
    }

    /// Where a [`Secret`]'s value may be read: each site, how many reads it
    /// makes, and what it hands the value to. A login handing it to the
    /// driver (sqlx's password, tiberius's AAD token), the token endpoint's
    /// refresh grant, and the connect's one consented store of the refresh
    /// token. Nothing else — no log, no error message, no stray write — can
    /// reach the value without adding a line here.
    const EXPOSE_SITES: &[(&str, &str, usize, &str)] = &[
        (
            "db/postgres.rs",
            "renew_login",
            1,
            "sqlx: the pool's password",
        ),
        (
            "db/sqlserver/pool.rs",
            "login_config",
            1,
            "tiberius: a new client's AAD token",
        ),
        (
            "db/sqlserver/pool.rs",
            "into_config",
            1,
            "tiberius: a login with one fixed token",
        ),
        (
            "ui/state/backend.rs",
            "login",
            1,
            "sqlx: the Postgres connect URL's password",
        ),
        (
            "azure.rs",
            "renew_silently",
            1,
            "the token endpoint's refresh grant",
        ),
        (
            "ui/state/connect.rs",
            "finish_entra_connect",
            1,
            "the refresh token, kept where the user's answer says",
        ),
    ];

    /// Every `.rs` file under `src/`, by path relative to it.
    fn sources() -> Vec<(String, String)> {
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
        out
    }

    /// A file's shipped code, with comments removed
    /// ([`crate::source_scan::code_outside_test_modules`]).
    fn shipped(source: &str) -> String {
        crate::source_scan::code_outside_test_modules(source)
    }

    /// Each read of a secret in `code` — `expose` as a whole identifier, but
    /// not its definition — by the function it sits in.
    fn expose_reads(code: &str) -> Vec<String> {
        let ident = |c: char| c.is_alphanumeric() || c == '_';
        code.match_indices("expose")
            .filter(|(at, _)| {
                !code[..*at].chars().next_back().is_some_and(ident)
                    && !code[at + "expose".len()..]
                        .chars()
                        .next()
                        .is_some_and(ident)
                    && !code[..*at].ends_with("fn ")
            })
            .map(|(at, _)| {
                let before = &code[..at];
                let fn_at = before
                    .match_indices("fn ")
                    .filter(|(i, _)| !before[..*i].chars().next_back().is_some_and(ident))
                    .last()
                    .map(|(i, _)| i + 3)
                    .expect("a read outside any function");
                before[fn_at..].chars().take_while(|c| ident(*c)).collect()
            })
            .collect()
    }

    #[test]
    fn a_secret_is_read_only_where_a_login_or_the_identity_provider_takes_it() {
        use std::collections::BTreeMap;
        let mut found: BTreeMap<(String, String), usize> = BTreeMap::new();
        for (file, source) in sources() {
            for function in expose_reads(&shipped(&source)) {
                *found.entry((file.clone(), function)).or_default() += 1;
            }
        }
        let expected: BTreeMap<(String, String), usize> = EXPOSE_SITES
            .iter()
            .map(|(file, function, reads, _)| ((file.to_string(), function.to_string()), *reads))
            .collect();
        assert_eq!(
            found, expected,
            "a token's value is read somewhere new, or no longer where it was: \
             every read hands it to a driver, the token endpoint or the \
             consented store, and is registered in EXPOSE_SITES with why"
        );
    }

    #[test]
    fn the_read_scan_attributes_and_counts() {
        let code = shipped(
            "fn a() { x.expose(); }\n\
             // fn b() { y.expose(); }\n\
             fn c() { let s = Secret::expose; z.expose(); } // .expose()\n\
             pub fn expose(&self) {}",
        );
        assert_eq!(expose_reads(&code), vec!["a", "c", "c"]);
    }
}
