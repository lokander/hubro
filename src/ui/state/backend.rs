//! The per-engine data the server connect flows are parameterized over
//! ([`ServerBackend`]), and the credential types a connect hands a backend.

use super::*;

use std::sync::Arc;

use crate::db::entra::{EntraTokens, RenewError, Renewed, Secret, Token};

/// The credential a saved connection offers from memory, and whether that is a
/// remembered password — derived from the credential itself, so the two cannot
/// disagree. [`password_retry`] reads a refusal as the remembered password
/// failing on the strength of the flag, so a flag that said "remembered"
/// while the open sent nothing would prompt on every first use — the server
/// refuses the empty login, and the refusal is pinned on the password — and
/// an unticked answer to that prompt deletes the stored one.
pub(super) fn remembered_credential(remembered: Option<&str>) -> (ServerCredential<'_>, bool) {
    let credential = match remembered {
        Some(password) => ServerCredential::Password(password),
        None => ServerCredential::None,
    };
    let offered = matches!(credential, ServerCredential::Password(_));
    (credential, offered)
}

/// A secret to hand a server backend at connect time. Which variant a connect
/// carries decides how the backend uses it: Postgres takes a password and an
/// Entra token the same way — spliced into the URL — while SQL Server splices
/// the password but hands the token to the driver as an AAD login, since a
/// token sent as a password is just a rejected password.
pub(super) enum ServerCredential<'a> {
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
pub(super) struct ServerLogin {
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
    pub(super) url_target: fn(&str) -> Result<(String, u16), DbError>,
    /// Rewrites a URL onto the tunnel's forwarded local port.
    pub(super) via_local_port: fn(&str, u16) -> Result<String, DbError>,
    /// The OAuth resource this engine's Entra tokens are minted for.
    pub(super) entra_resource: &'static str,
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
    pub(super) fn tls_host(&self, url: &str, tunneled: bool) -> Option<String> {
        match self.kind {
            BackendKind::SqlServer if tunneled => (self.url_target)(url).ok().map(|(host, _)| host),
            _ => None,
        }
    }

    /// Where `credential` goes for this engine: into the URL as its password,
    /// or to the driver as an AAD login. Pure — see [`ServerLogin`] for why
    /// this is a step of its own.
    pub(super) fn login(
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
    pub(super) async fn open(
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
    pub(super) fn saved(
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
pub(super) fn entra_renewal(
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

#[cfg(test)]
mod tests {
    use super::*;

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
}
