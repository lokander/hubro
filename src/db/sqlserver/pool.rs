//! Connecting to SQL Server and running statements on a pooled tiberius
//! client: URL parsing into a driver [`Config`], the hand-rolled pool, the
//! query/execute/export entry points, and script transactions.
//!
//! ## Pooling
//!
//! tiberius has no pool of its own and a [`tiberius::Client`] is `&mut self`
//! for every query, so this module hand-rolls a small pool: up to
//! [`MAX_CONNECTIONS`] clients created on demand, idle ones kept in a `Vec`
//! behind a sync `Mutex`, concurrency limited by a tokio `Semaphore`. That was
//! chosen over `bb8-tiberius`/`deadpool-tiberius` because the app needs so
//! little (matching PgPool's max-4 behavior, an *owned* checkout for script
//! transactions, and an explicit close) that a dependency-free ~100 lines is
//! simpler than adapting a general-purpose pool. Broken connections are
//! discarded instead of returned: any driver error other than a server-raised
//! SQL error ([`tiberius::error::Error::Server`]) may leave the TDS stream in
//! an undefined state.
//!
//! ## Transactions
//!
//! `BEGIN TRAN`/`COMMIT`/`ROLLBACK` run as raw batches on one checked-out
//! connection ([`MssqlTx`]), which the server ties together via the session's
//! transaction descriptor. This predates tiberius 0.13's transaction API
//! (TDS Transaction Manager requests), which nothing here uses.

use std::io::Write;
use std::ops::ControlFlow;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures_util::TryStreamExt as _;
use tiberius::error::Error as TdsError;
use tiberius::{AuthMethod, Client, Column, Config, EncryptionLevel, Query, QueryItem, Row};
use tokio::net::TcpStream;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio_util::compat::{Compat, TokioAsyncWriteCompatExt as _};

use super::{column_infos, decode_row};
use crate::db::error::DbError;
use crate::db::export::{export_io_err, ExportFormat, ExportSink};
use crate::db::registry::RolledBack;
use crate::db::script::{
    final_read_may_close, statement_needs, tsql_is_transaction_control, tsql_leads_own_batch,
};
use crate::db::staged::CheckedStatement;
use crate::db::value::{cap_value, ColumnInfo, QueryResult, Value};

/// Matches PgPool's `max_connections(4)`: enough for the grid, introspection,
/// and a script transaction to run concurrently without deadlocking.
const MAX_CONNECTIONS: usize = 4;

/// How long one connect may take, from dialing the socket to the end of the
/// session setup — an Azure routing redirect and its second login included —
/// before it is abandoned: the bound a Postgres open already has
/// (sqlx's 30s acquire timeout). tiberius's own 15s handshake timeout covers
/// only what follows the TCP connect, and that connect has no bound of its
/// own: measured, a host that drops SYNs held a connect for 133s on Linux,
/// and with it the whole connections row and anything queued behind it.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

/// How long an idle connection may answer its liveness check before it is
/// presumed dead and replaced. A connection a NAT or firewall dropped without
/// telling either end never answers at all, so the check needs a bound of its
/// own or it would trade one failed query for a hang.
const PING_TIMEOUT: Duration = Duration::from_secs(5);

/// A connection back in the pool for less than this is handed out without a
/// check: it answered a moment ago. A schema load or a page of the grid takes
/// connections in quick succession, and a round trip each would add up on a
/// distant server. The cost is that a session killed within this window still
/// fails one query.
const CHECK_AFTER: Duration = Duration::from_secs(1);

/// Idle longer than this and a connection is closed rather than checked: the
/// Postgres pool's `idle_timeout` (sqlx's default). By then a middlebox may
/// well have forgotten it silently, and a check would only wait out
/// [`PING_TIMEOUT`] to find that out.
const IDLE_TIMEOUT: Duration = Duration::from_secs(600);

/// Session options every pooled connection runs right after login. tiberius
/// logs in with the ODBC option flag, whose server-side defaults already
/// include `QUOTED_IDENTIFIER ON`, but nothing in the login *guarantees* it —
/// and the app's identifier quoting is ANSI double quotes everywhere, which
/// only parses with QUOTED_IDENTIFIER ON. Set it (and the ANSI family SQL
/// Server's own drivers set) explicitly so the session never depends on
/// server or database defaults. TEXTSIZE lifts the legacy cap on
/// text/ntext/image reads.
///
/// NOCOUNT OFF is what every guarded write's row count stands on
/// ([`statement_rows`]). A server whose `user options` turn NOCOUNT on for
/// new sessions sends no counts at all — measured, with 512 configured, a
/// one-row update reported none, so every edit read as zero rows and was
/// refused.
///
/// No statement can undo this pin for the ones after it. A trigger's `SET`
/// lapses when the trigger returns, and so does a script's: scripts run
/// through `sp_executesql`, and a `SET` inside it ends with the call. The one
/// exception is a statement that must lead its batch ([`tsql_leads_own_batch`]:
/// `CREATE`/`ALTER` of a procedure, function, trigger or view, `CREATE
/// SCHEMA`/`DEFAULT`/`RULE`), which runs as a plain batch, where a `SET` would
/// stick. It is safe because a `SET` trailing one of those never runs as the
/// session's own: measured, a procedure or trigger takes it into its body, and
/// the rest refuse it as a syntax error (156). The other plain batch is bare
/// transaction control ([`tsql_is_transaction_control`]): the keywords and at
/// most a transaction's name, with a reserved word, a parenthesis or a string
/// in the name's place turned away — so a `SET`, or an `EXEC` that could carry
/// one, never rides along.
const SESSION_SETUP: &str = "SET QUOTED_IDENTIFIER ON; \
     SET NOCOUNT OFF; \
     SET ANSI_NULLS ON; \
     SET ANSI_PADDING ON; \
     SET ANSI_WARNINGS ON; \
     SET CONCAT_NULL_YIELDS_NULL ON; \
     SET TEXTSIZE 2147483647;";

type TdsClient = Client<Compat<TcpStream>>;

/// How a SQL Server connect authenticates. [`open_mssql`] always uses
/// `Password`; the Entra flow passes `AadToken` to [`open_mssql_with`].
#[derive(Clone)]
pub enum MssqlAuth {
    /// SQL Server authentication with the URL's user and (possibly spliced-in)
    /// password.
    Password,
    /// Microsoft Entra ID: log in with this AAD access token; the URL's
    /// user/password are ignored.
    AadToken(String),
}

impl std::fmt::Debug for MssqlAuth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MssqlAuth::Password => write!(f, "Password"),
            MssqlAuth::AadToken(_) => write!(f, "AadToken(<HIDDEN>)"),
        }
    }
}

/// The connection settings parsed out of an mssql URL, one step before the
/// driver's opaque [`Config`] so parsing stays unit-testable.
#[derive(Debug, Clone, PartialEq)]
struct MssqlUrlParts {
    host: String,
    port: u16,
    user: String,
    password: String,
    database: Option<String>,
    encryption: EncryptionLevel,
    trust_server_certificate: bool,
}

/// Parses an mssql URL (`mssql://user@host:port/db?encrypt=…`) into its
/// connection settings. Recognized query params: `encrypt`
/// (`on`/`off`/`plaintext`) and `trustServerCertificate` (`true`/`false`);
/// anything else is rejected so a typo can't silently weaken TLS settings.
fn parse_mssql_url(url: &str) -> Result<MssqlUrlParts, DbError> {
    let parsed = url::Url::parse(url).map_err(|e| DbError::Connect(format!("invalid URL: {e}")))?;
    if parsed.scheme() != "mssql" && parsed.scheme() != "sqlserver" {
        return Err(DbError::Connect(format!(
            "expected an mssql:// URL, got {}://",
            parsed.scheme()
        )));
    }
    let host = parsed
        .host_str()
        .ok_or_else(|| DbError::Connect("URL has no host".into()))?
        .trim_matches(['[', ']'])
        .to_string();
    let decode = |s: &str| -> Result<String, DbError> {
        percent_encoding::percent_decode_str(s)
            .decode_utf8()
            .map(|s| s.into_owned())
            .map_err(|_| DbError::Connect("URL contains invalid percent-encoding".into()))
    };
    let user = decode(parsed.username())?;
    let password = match parsed.password() {
        Some(p) => decode(p)?,
        None => String::new(),
    };
    let database = {
        let path = parsed.path().trim_start_matches('/');
        (!path.is_empty()).then(|| decode(path)).transpose()?
    };
    // Defaults mirror the driver's own with TLS compiled in: encrypt
    // everything and fail if the server can't.
    let mut encryption = EncryptionLevel::Required;
    let mut trust_server_certificate = false;
    for (key, value) in parsed.query_pairs() {
        match key.to_ascii_lowercase().as_str() {
            "encrypt" => {
                encryption = match value.to_ascii_lowercase().as_str() {
                    "on" | "true" | "yes" | "required" => EncryptionLevel::Required,
                    // "off" still encrypts the login packet, matching the
                    // ADO.NET `Encrypt=false` behavior.
                    "off" | "false" | "no" => EncryptionLevel::Off,
                    "plaintext" | "danger_plaintext" => EncryptionLevel::NotSupported,
                    other => {
                        return Err(DbError::Connect(format!(
                            "invalid encrypt value: {other} (expected on, off, or plaintext)"
                        )))
                    }
                };
            }
            "trustservercertificate" => {
                trust_server_certificate = match value.to_ascii_lowercase().as_str() {
                    "true" | "yes" | "1" => true,
                    "false" | "no" | "0" => false,
                    other => {
                        return Err(DbError::Connect(format!(
                            "invalid trustServerCertificate value: {other}"
                        )))
                    }
                };
            }
            other => {
                return Err(DbError::Connect(format!(
                    "unsupported URL parameter: {other}"
                )))
            }
        }
    }
    Ok(MssqlUrlParts {
        host,
        port: parsed.port().unwrap_or(1433),
        user,
        password,
        database,
        encryption,
        trust_server_certificate,
    })
}

impl MssqlUrlParts {
    /// Builds the driver config. `tls_host`, when set, replaces the URL's host
    /// in the config — tiberius takes the TLS server name (SNI + certificate
    /// validation) from the config's host, while hubro dials the TCP socket
    /// itself, so an SSH-tunneled connect can dial `127.0.0.1:<forwarded>` yet
    /// still validate the server's certificate against its real hostname.
    fn into_config(self, auth: &MssqlAuth, tls_host: Option<&str>) -> Config {
        let mut config = Config::new();
        config.host(tls_host.unwrap_or(&self.host));
        config.port(self.port);
        match auth {
            MssqlAuth::Password => {
                config.authentication(AuthMethod::sql_server(&self.user, &self.password));
            }
            MssqlAuth::AadToken(token) => {
                config.authentication(AuthMethod::aad_token(token));
            }
        }
        if let Some(database) = &self.database {
            config.database(database);
        }
        config.encryption(self.encryption);
        if self.trust_server_certificate {
            config.trust_cert();
        }
        config.application_name("hubro");
        // tiberius 0.13 bounds every server round-trip at 30s by default and
        // drops the connection when it fires, so a long user query (a big sort,
        // `WAITFOR`, a blocked lock) would fail at 30s — measured, not assumed.
        // Postgres sets no statement timeout either. The 15s handshake bound
        // stays: it only covers prelogin, TLS and login.
        config.command_timeout(None);
        config
    }
}

/// A small fixed-size pool of tiberius clients (see the module docs for why
/// it's hand-rolled). Cheap to clone — the state lives behind an `Arc` — so
/// async tasks can grab a copy instead of borrowing state across an await.
#[derive(Clone)]
pub struct MssqlPool {
    inner: Arc<PoolInner>,
}

struct PoolInner {
    config: Config,
    /// The TCP address new connections dial. Matches the config's host/port
    /// for a direct connect; differs when an SSH tunnel forwards the
    /// connection — then this is `127.0.0.1:<forwarded port>` while the config
    /// keeps the logical host for TLS validation (see
    /// [`MssqlUrlParts::into_config`]).
    addr: (String, u16),
    /// Bounds live connections at [`MAX_CONNECTIONS`]; a checkout holds one
    /// permit for its whole lifetime.
    permits: Arc<Semaphore>,
    /// Connected clients not currently checked out. Sync mutex: never held
    /// across an await.
    idle: Mutex<Vec<IdleClient>>,
    closed: AtomicBool,
}

/// A connected client waiting in the pool, and when it was put there.
struct IdleClient {
    client: TdsClient,
    since: Instant,
}

/// What [`MssqlPool::acquire`] does with an idle connection before handing it
/// out. The server, or an Azure gateway, may have closed it while it waited,
/// and a connection the pool cannot vouch for fails the next query instead —
/// measured: after a `KILL` of an idle session, the next query failed with an
/// I/O error and the one after it worked. Postgres never shows this, because
/// sqlx checks every connection before handing it out (`test_before_acquire`).
#[derive(Debug, PartialEq, Eq)]
enum IdleReuse {
    /// Used a moment ago ([`CHECK_AFTER`]): hand it out as it is.
    Reuse,
    /// Run a trivial query first, and reuse it only if that answers in time.
    Check,
    /// Too old to be worth checking: close it and take the next.
    Discard,
}

fn idle_reuse(idle_for: Duration) -> IdleReuse {
    if idle_for < CHECK_AFTER {
        IdleReuse::Reuse
    } else if idle_for > IDLE_TIMEOUT {
        IdleReuse::Discard
    } else {
        IdleReuse::Check
    }
}

/// How an idle connection's check went.
#[derive(Debug, PartialEq, Eq)]
enum Liveness {
    Alive,
    /// It answered with an error, or its socket was closed: this one is gone.
    Failed,
    /// No answer at all within the bound. Whatever silenced it — a NAT or
    /// firewall that forgot the connection, a network gone away — has almost
    /// certainly silenced its idle siblings too, so they are dropped
    /// unchecked rather than each waiting out [`PING_TIMEOUT`] in turn.
    Silent,
}

/// Checks that `client` still answers, within `limit`.
async fn liveness(client: &mut TdsClient, limit: Duration) -> Liveness {
    match tokio::time::timeout(limit, run_batch(client, "SELECT 1")).await {
        Ok(Ok(())) => Liveness::Alive,
        Ok(Err(_)) => Liveness::Failed,
        Err(_) => Liveness::Silent,
    }
}

/// One checked-out connection. Returns itself to the pool on drop unless it
/// was [`discarded`](Self::discard) or the pool has closed.
struct PooledConn {
    client: Option<TdsClient>,
    pool: Arc<PoolInner>,
    _permit: OwnedSemaphorePermit,
}

impl PooledConn {
    fn client(&mut self) -> &mut TdsClient {
        self.client.as_mut().expect("connection already discarded")
    }

    /// True once [`Self::discard`] ran. A [`MssqlTx`] can outlive its
    /// connection this way (a fatal mid-script error discards it), so the
    /// transaction entry points must check before touching the client.
    fn is_discarded(&self) -> bool {
        self.client.is_none()
    }

    /// Drops the client instead of returning it to the pool — for connections
    /// whose TDS stream may be left mid-response (driver-level errors, capped
    /// reads that abandoned a result set).
    fn discard(&mut self) {
        self.client = None;
    }
}

/// The error for operations on a transaction whose connection was already
/// discarded by an earlier fatal failure. Not a new failure: the server rolls
/// the abandoned transaction back when the dead connection goes away.
fn lost_connection() -> DbError {
    DbError::Query("the connection was lost — the transaction was rolled back by the server".into())
}

impl Drop for PooledConn {
    fn drop(&mut self) {
        if let Some(client) = self.client.take() {
            if !self.pool.closed.load(Ordering::SeqCst) {
                self.pool
                    .idle
                    .lock()
                    .expect("pool lock poisoned")
                    .push(IdleClient {
                        client,
                        since: Instant::now(),
                    });
            }
            // Pool closed: dropping the client closes the socket. The graceful
            // TDS logout needs an async context the destructor doesn't have.
        }
    }
}

impl MssqlPool {
    fn new(config: Config, addr: (String, u16)) -> Self {
        MssqlPool {
            inner: Arc::new(PoolInner {
                config,
                addr,
                permits: Arc::new(Semaphore::new(MAX_CONNECTIONS)),
                idle: Mutex::new(Vec::new()),
                closed: AtomicBool::new(false),
            }),
        }
    }

    /// Checks a connection out, connecting a fresh client when no idle one is
    /// available (up to [`MAX_CONNECTIONS`] total; further callers wait). An
    /// idle one is handed out only once it has answered a check
    /// ([`idle_reuse`]); one that does not is dropped for the next.
    async fn acquire(&self) -> Result<PooledConn, DbError> {
        let permit = Arc::clone(&self.inner.permits)
            .acquire_owned()
            .await
            .map_err(|_| DbError::Query("the connection is closed".into()))?;
        let client = loop {
            // A statement of its own, so the guard is gone before any await.
            let idle = self.inner.idle.lock().expect("pool lock poisoned").pop();
            let Some(IdleClient { mut client, since }) = idle else {
                break connect_client(&self.inner.config, &self.inner.addr, CONNECT_TIMEOUT)
                    .await?;
            };
            match idle_reuse(since.elapsed()) {
                IdleReuse::Reuse => break client,
                IdleReuse::Check => match liveness(&mut client, PING_TIMEOUT).await {
                    Liveness::Alive => break client,
                    Liveness::Failed => {}
                    Liveness::Silent => {
                        self.inner.idle.lock().expect("pool lock poisoned").clear();
                    }
                },
                IdleReuse::Discard => {}
            }
            // Dropping the client closes its socket; nothing to log out of on
            // a session that is already gone.
        };
        Ok(PooledConn {
            client: Some(client),
            pool: Arc::clone(&self.inner),
            _permit: permit,
        })
    }

    /// Closes the pool: idle connections are logged out, checked-out ones are
    /// dropped (not returned) when their tasks finish, and new acquires fail.
    pub async fn close(&self) {
        self.inner.closed.store(true, Ordering::SeqCst);
        self.inner.permits.close();
        let clients: Vec<IdleClient> = {
            let mut idle = self.inner.idle.lock().expect("pool lock poisoned");
            idle.drain(..).collect()
        };
        for IdleClient { client, .. } in clients {
            let _ = client.close().await;
        }
    }
}

/// Connects to SQL Server from a URL (`mssql://user@host:port/db?encrypt=…`).
/// The URL may carry a password; saved config never does — callers splice a
/// session password in via [`super::mssql_url_with_password`]. Validates the
/// connection with a `SELECT 1` round-trip.
pub async fn open_mssql(url: &str) -> Result<MssqlPool, DbError> {
    open_mssql_with(url, &MssqlAuth::Password, None).await
}

/// [`open_mssql`] with explicit auth and an optional TLS host override.
/// `tls_host` is the server's logical hostname for an SSH-tunneled
/// connect whose URL was rewritten to `127.0.0.1:<forwarded>`: the socket
/// dials the URL's host/port, while TLS (SNI + certificate validation) uses
/// `tls_host` — so `encrypt=on` keeps validating the real certificate through
/// the tunnel.
pub async fn open_mssql_with(
    url: &str,
    auth: &MssqlAuth,
    tls_host: Option<&str>,
) -> Result<MssqlPool, DbError> {
    let parts = parse_mssql_url(url)?;
    let addr = (parts.host.clone(), parts.port);
    let config = parts.into_config(auth, tls_host);
    let pool = MssqlPool::new(config, addr);
    {
        let mut conn = pool.acquire().await?;
        run_query(conn.client(), "SELECT 1", &[])
            .await
            .map_err(|e| {
                // A failed validation never returns the connection to the pool.
                conn.discard();
                connect_error(&e)
            })?;
        // conn drops here and seeds the idle list.
    }
    Ok(pool)
}

/// Opens a TCP connection to `addr` and performs the TDS login, following at
/// most one Azure-style routing redirect, then applies [`SESSION_SETUP`] —
/// all of it within `limit` ([`CONNECT_TIMEOUT`]). The timeout is reported as
/// an I/O timeout so [`friendly_connect_error`] files it with the network
/// errors.
async fn connect_client(
    config: &Config,
    addr: &(String, u16),
    limit: Duration,
) -> Result<TdsClient, DbError> {
    match tokio::time::timeout(limit, connect_unbounded(config, addr)).await {
        Ok(result) => result,
        Err(_) => Err(connect_error(&TdsError::Io {
            kind: std::io::ErrorKind::TimedOut,
            message: format!("no answer from {}:{} within {limit:?}", addr.0, addr.1),
        })),
    }
}

/// [`connect_client`] without its bound.
async fn connect_unbounded(config: &Config, addr: &(String, u16)) -> Result<TdsClient, DbError> {
    let mut client = match connect_once(config.clone(), addr).await {
        // Azure SQL can answer the login with "actually, talk to this other
        // node"; a single redirect is all the protocol calls for. The redirect
        // target is dialed directly — it names a reachable gateway node.
        Err(TdsError::Routing { host, port }) => {
            let mut redirected = config.clone();
            redirected.host(&host);
            redirected.port(port);
            let addr = (host, port);
            connect_once(redirected, &addr).await
        }
        other => other,
    }
    .map_err(|e| connect_error(&e))?;
    match run_batch(&mut client, SESSION_SETUP).await {
        Ok(()) => Ok(client),
        Err(e) => Err(connect_error(&e)),
    }
}

async fn connect_once(config: Config, addr: &(String, u16)) -> Result<TdsClient, TdsError> {
    let io_err = |e: std::io::Error| TdsError::Io {
        kind: e.kind(),
        message: e.to_string(),
    };
    let tcp = TcpStream::connect((addr.0.as_str(), addr.1))
        .await
        .map_err(io_err)?;
    tcp.set_nodelay(true).map_err(io_err)?;
    Client::connect(config, tcp.compat_write()).await
}

/// SQL Server's "Login failed for user '…'", its wrong-credentials code. Sent
/// for a wrong password, an unknown login, and an empty password where one is
/// required — measured, all three — and, ahead of anything about the
/// database, for a wrong password naming a database that would also have
/// failed. Also sent, measured, where the password was right: a
/// contained-database user naming a missing database, and a login denied
/// `CONNECT SQL` (see [`DbError::CredentialsRejected`]).
///
/// **The fix for 4060 depends on tiberius keeping the first ERROR token.** A
/// database that cannot be opened draws *two*: 4060, then "Login failed for
/// user '…'", 18456's text (sqlcmd prints both against SQL Server 2022). tiberius
/// 0.13 keeps the first — `TokenStream::flush_done`, `tds/stream/token.rs`
/// lines 197-199 — so `Error::code()` is 4060. A tiberius that kept the last
/// token instead would make every such failure an 18456 and put the needless
/// prompt back, with only the engine suite (`tests/db_sqlserver.rs`, not run by CI)
/// to notice.
const LOGIN_FAILED: u32 = 18456;

/// "Cannot open database "…" requested by the login. The login failed." Sent
/// only once the credentials were accepted, for a database that does not
/// exist, is offline or restoring, or that this login may not open.
const CANNOT_OPEN_DATABASE: u32 = 4060;

/// Turns a failed login or session setup into the error the connect flow
/// acts on. See [`connect_error_from`].
fn connect_error(err: &TdsError) -> DbError {
    connect_error_from(err.code(), friendly_connect_error(err))
}

/// The classification itself, over plain data so a test can execute it (a
/// tiberius `TokenError` cannot be built outside tiberius).
///
/// Decided on the server's error code and nothing else: the connect flow
/// prompts for a new password on [`DbError::CredentialsRejected`] (whose
/// answer can replace or delete the stored one), and text is shared between
/// codes — 4060's own message ends in "The login failed.", which is how a
/// missing database used to delete a working password. Only
/// 18456 — the server's wrong-credentials code, not a guarantee the password
/// was wrong (see [`LOGIN_FAILED`]) — qualifies. Other codes in that family —
/// a disabled account (18470, measured) — refuse a password that is still
/// correct, so they keep it and report.
fn connect_error_from(code: Option<u32>, message: String) -> DbError {
    if code == Some(LOGIN_FAILED) {
        DbError::CredentialsRejected(message)
    } else {
        DbError::Connect(message)
    }
}

/// Categorizes common failure modes so the connections screen reads well:
/// auth, network/DNS, TLS, wrong-server.
///
/// Words only. Whether the password is to blame — the question that costs a
/// stored secret — is [`connect_error_from`]'s, on the error code; nothing
/// may decide it from this text.
fn friendly_connect_error(err: &TdsError) -> String {
    let msg = err.to_string();
    let lower = msg.to_lowercase();
    // The database arm comes first: 4060's message contains "the login failed"
    // as well, and it is not a password problem.
    if err.code() == Some(CANNOT_OPEN_DATABASE) || lower.contains("cannot open database") {
        format!(
            "cannot open the database — it may not exist, be offline or restoring, or this \
             login may lack access to it; the credentials were accepted — {msg}"
        )
    } else if lower.contains("login failed") {
        format!("authentication failed — {msg}")
    } else if matches!(err, TdsError::Protocol(_))
        || matches!(
            err,
            TdsError::Io {
                kind: std::io::ErrorKind::UnexpectedEof,
                ..
            }
        )
    {
        // The TDS handshake got an answer tiberius couldn't parse, or the
        // server hung up on the prelogin (Postgres does — observed as an
        // UnexpectedEof "failed to fill whole buffer") — usually a different
        // database server on that host/port. The mirror of the Postgres-side
        // "doesn't appear to be Postgres" hint.
        format!("the server doesn't appear to be SQL Server — check the host and port — {msg}")
    } else if matches!(
        err,
        TdsError::Io {
            kind: std::io::ErrorKind::TimedOut,
            ..
        }
    ) {
        // Ahead of the TLS arm: tiberius's handshake-timeout message names
        // "TLS negotiation" among its stages without TLS having failed, and
        // ends in advice for whoever configures the driver, so it gets our own
        // words. A TCP connect timeout lands here too, and keeps the OS's text.
        if lower.contains("handshake") {
            "network error — the server accepted the connection but did not finish \
             the SQL Server login handshake in time"
                .to_string()
        } else {
            format!("network error — {msg}")
        }
    } else if matches!(err, TdsError::Tls(_)) || lower.contains("tls") || lower.contains("ssl") {
        format!("TLS error — {msg}")
    } else if lower.contains("connection refused")
        || lower.contains("timed out")
        || lower.contains("failed to lookup")
        || lower.contains("no such host")
        || lower.contains("connection reset")
    {
        format!("network error — {msg}")
    } else {
        msg
    }
}

/// True when the error may have left the TDS stream mid-response, making the
/// connection unsafe to reuse. Server-raised SQL errors (syntax errors,
/// constraint violations, …) arrive as ordinary tokens in a well-formed
/// response and leave the connection healthy.
fn is_fatal(err: &TdsError) -> bool {
    !matches!(err, TdsError::Server(_))
}

/// Resolves a driver-level result against the checked-out connection: errors
/// that may have corrupted the stream discard the connection instead of
/// returning it to the pool.
fn settle<T>(conn: &mut PooledConn, result: Result<T, TdsError>, sql: &str) -> Result<T, DbError> {
    result.map_err(|e| {
        if is_fatal(&e) {
            conn.discard();
        }
        query_error(&e, sql)
    })
}

/// Maps a driver error onto [`DbError::Query`]. No position post-processing:
/// SQL Server reports the failing batch line inside the message itself
/// (tiberius renders `'…' on server … on line N`).
fn query_error(err: &TdsError, _sql: &str) -> DbError {
    DbError::Query(err.to_string())
}

/// Binds backend-neutral [`Value`] parameters onto a [`Query`].
///
/// NULL caveat: `Value::Null` binds as `None::<String>`, i.e. an
/// `nvarchar` NULL — the same choice as the Postgres backend's text NULL.
/// Fine for the current uses (filter values are text); the staged SQL
/// builders render NULL inline as the literal `NULL`, so a NULL never
/// reaches a typed column through this function.
fn bind_params(query: &mut Query<'_>, params: &[Value]) {
    for param in params {
        match param {
            Value::Null => query.bind(Option::<String>::None),
            Value::Integer(i) => query.bind(*i),
            Value::Real(r) => query.bind(*r),
            Value::Text(t) => query.bind(t.clone()),
            Value::Blob(b) => query.bind(b.clone()),
        }
    }
}

/// Runs a parameterized query and buffers the first result set. The bool is
/// whether the stream was abandoned with data still unread (a second result
/// set arrived): tiberius would silently drain the leftovers before the next
/// query on this connection, so pool-owning callers must discard it.
async fn run_query(
    client: &mut TdsClient,
    sql: &str,
    params: &[Value],
) -> Result<(QueryResult, bool), TdsError> {
    // No caps: `u64::MAX`/`usize::MAX` make both bounds unreachable, so this
    // is the capped read with its two limiters switched off rather than a
    // second copy of the same loop.
    let (result, _, abandoned) =
        run_query_capped(client, sql, params, u64::MAX, usize::MAX).await?;
    Ok((result, abandoned))
}

/// What [`read_first_result_set`] does with each row it reads.
trait RowSink {
    /// Takes one row; `Break` stops reading with the rest of the response
    /// still unread.
    fn row(&mut self, row: &Row) -> Result<ControlFlow<()>, TdsError>;
}

/// Runs a parameterized query and feeds its first result set to the sink
/// `start` builds from that set's columns (`None`: the batch returned no
/// result set). The one copy of the TDS result loop: the buffered, capped and
/// export paths differ only in their sinks.
///
/// Returns the sink and whether the stream was abandoned with data still
/// unread — the sink stopped early, or a second result set arrived. Only the
/// first is read, which mirrors the sqlx backends, where one statement yields
/// one result.
async fn read_first_result_set<S: RowSink>(
    client: &mut TdsClient,
    sql: &str,
    params: &[Value],
    start: impl FnOnce(Option<&[Column]>) -> Result<S, TdsError>,
) -> Result<(S, bool), TdsError> {
    let mut query = Query::new(sql.to_string());
    bind_params(&mut query, params);
    let mut stream = query.query(client).await?;
    let mut sink = start(stream.columns().await?)?;
    let mut result_sets = 0usize;
    while let Some(item) = stream.try_next().await? {
        match item {
            QueryItem::Metadata(_) => {
                result_sets += 1;
                if result_sets > 1 {
                    return Ok((sink, true));
                }
            }
            QueryItem::Row(row) => {
                if sink.row(&row)?.is_break() {
                    return Ok((sink, true));
                }
            }
        }
    }
    Ok((sink, false))
}

/// Runs a parameterized non-row statement via the driver's RPC path,
/// returning every affected-row count its response carried, in arrival order:
/// one per statement the server ran on its behalf, *triggers included*. Read
/// it with [`statement_rows`] or sum it, and say which.
async fn run_execute(
    client: &mut TdsClient,
    sql: &str,
    params: &[Value],
) -> Result<Vec<u64>, TdsError> {
    let mut query = Query::new(sql.to_string());
    bind_params(&mut query, params);
    let result = query.execute(client).await?;
    Ok(result.into_iter().collect())
}

/// The rows one statement itself affected, out of every count its response
/// carried — the number the guarded writes hold to an expected count, which
/// on Postgres (one command tag) and SQLite (`changes()`) already leaves
/// trigger work out.
///
/// Here it does not come apart by kind: through `sp_executesql` the
/// statement's count and every count from DML its triggers ran arrive as the
/// same `DONEINPROC` token. It comes apart by order. A trigger runs inside the
/// statement that fired it, so its statements finish, and are reported,
/// first; the statement's own count is the last. Measured against SQL Server
/// 2022 for `AFTER INSERT`/`UPDATE`/`DELETE` triggers that write more rows
/// than the statement does, a nested trigger, a trigger returning a result
/// set, and `INSTEAD OF` triggers: always last, always the statement's. An
/// `INSTEAD OF` trigger replaces the statement, whose count is then the rows
/// the trigger fired for — what the WHERE clause matched, not what the
/// trigger went on to write.
///
/// Not every count is a count. A statement that completes without counting
/// rows — inside a trigger, a `SET` for one — still sends a `DONEINPROC`, and
/// tiberius reports it as 0, so zeros are mixed in among the triggers'
/// counts. They come from inside the triggers, so they always arrive before
/// the statement's own count, which is still the last.
///
/// Only sound while the session reports counts at all: under `NOCOUNT ON`
/// the statement sends none, and the last count would be a trigger's that
/// turned `NOCOUNT` off for itself. [`SESSION_SETUP`] pins it off. No count
/// reads as zero rows, which no guarded write expects.
fn statement_rows(counts: &[u64]) -> u64 {
    counts.last().copied().unwrap_or(0)
}

/// Runs one script statement that returns no rows, reporting its affected
/// rows. A statement SQL Server only accepts at the head of a batch
/// ([`tsql_leads_own_batch`]) cannot go through `sp_executesql`, so it runs as
/// a plain batch instead; nothing it does counts as an affected row, so it
/// reports 0.
///
/// Bare transaction control (`BEGIN TRAN`, `COMMIT`, `ROLLBACK`:
/// [`tsql_is_transaction_control`]) runs as a plain batch too. Inside
/// `sp_executesql` it fails with error 266 — the RPC refuses to return with
/// `@@TRANCOUNT` different from what it started with — and measured, the
/// `BEGIN TRAN` that error is about has still opened its transaction, so a
/// script could not open one and was left with one open anyway.
///
/// The report is the *sum* of every count, trigger rows included — unlike
/// [`statement_rows`], and deliberately: a script statement is a whole `GO`
/// batch, and its several statements' counts arrive as the same token as
/// their triggers', so no rule over the counts can tell the batch's own rows
/// from theirs. The sum is what SQL Server reported; nothing is held to it.
async fn run_statement(client: &mut TdsClient, sql: &str) -> Result<u64, TdsError> {
    if tsql_leads_own_batch(sql) || tsql_is_transaction_control(sql) {
        run_batch(client, sql).await.map(|()| 0)
    } else {
        run_execute(client, sql, &[])
            .await
            .map(|counts| counts.iter().sum())
    }
}

/// Runs a raw SQL batch (no parameters, results discarded) — the vehicle for
/// `BEGIN TRAN`/`COMMIT`/`ROLLBACK` and session `SET` options, which must run
/// as plain batches rather than through `sp_executesql`.
async fn run_batch(client: &mut TdsClient, sql: &str) -> Result<(), TdsError> {
    let stream = client.simple_query(sql).await?;
    stream.into_results().await?;
    Ok(())
}

pub async fn query_with(
    pool: &MssqlPool,
    sql: &str,
    params: &[Value],
) -> Result<QueryResult, DbError> {
    let mut conn = pool.acquire().await?;
    let result = run_query(conn.client(), sql, params).await;
    let (result, abandoned) = settle(&mut conn, result, sql)?;
    // Unread result sets left on the stream must not ride back to the pool —
    // the next borrower would pay for draining them.
    if abandoned {
        conn.discard();
    }
    Ok(result)
}

/// Executes a statement without decoding rows, returning the driver's
/// affected-row count.
pub async fn execute(pool: &MssqlPool, sql: &str) -> Result<u64, DbError> {
    let mut conn = pool.acquire().await?;
    let result = run_statement(conn.client(), sql).await;
    settle(&mut conn, result, sql)
}

/// Streams `sql`, decoding and retaining at most `max_rows` rows and capping
/// each cell to `cell_cap` bytes, so the free-form query path never scales
/// with table or value size. Returns the (bounded) result and
/// whether more rows existed beyond the cap.
///
/// A truncated read (or an abandoned second result set) leaves the rest of
/// the server's response mid-stream; tiberius would silently drain it before
/// the *next* query on that connection, which for a huge result would defer
/// the whole download to some innocent later query — so the connection is
/// discarded instead (one reconnect is far cheaper). Unless the statement may
/// write: a write that returns rows ends only once its response is read, so
/// then the rest is read here.
pub async fn query_capped(
    pool: &MssqlPool,
    sql: &str,
    params: &[Value],
    max_rows: u64,
    cell_cap: usize,
) -> Result<(QueryResult, bool), DbError> {
    let mut conn = pool.acquire().await?;
    let result = run_query_capped(conn.client(), sql, params, max_rows, cell_cap).await;
    let (result, truncated, abandoned) = settle(&mut conn, result, sql)?;
    if truncated || abandoned {
        let needs = statement_needs(sql, crate::db::sql::Dialect::SqlServer);
        if needs.mutate || needs.ddl {
            // A write that returns rows (`INSERT … OUTPUT`, or a batch that
            // writes after a long SELECT) only finishes once its response has
            // been read; discarding the connection here aborted it. Read the
            // rest, and report an error it ends in.
            let drained = run_batch(conn.client(), "SELECT 1").await;
            settle(&mut conn, drained, sql)?;
        } else {
            conn.discard();
        }
    }
    Ok((result, truncated))
}

/// [`query_capped`] against the connection held by a script transaction — the
/// read path for statements inside an atomically-wrapped script. No
/// bound params: scripts are raw text. Unlike the pool path, a truncated read
/// keeps the connection (discarding it would kill the open transaction); the
/// deferred drain of the abandoned rows is the price of atomicity.
pub async fn query_capped_conn(
    tx: &mut MssqlTx,
    sql: &str,
    max_rows: u64,
    cell_cap: usize,
) -> Result<(QueryResult, bool), DbError> {
    query_capped_held(&mut tx.conn, sql, max_rows, cell_cap).await
}

/// [`query_capped_conn`] on any connection held across statements. A
/// truncated read keeps the connection for the same reason in both: the
/// script may have a transaction of its own open on it.
async fn query_capped_held(
    conn: &mut PooledConn,
    sql: &str,
    max_rows: u64,
    cell_cap: usize,
) -> Result<(QueryResult, bool), DbError> {
    if conn.is_discarded() {
        return Err(lost_connection());
    }
    let result = run_query_capped(conn.client(), sql, &[], max_rows, cell_cap).await;
    let (result, truncated, _abandoned) = settle(conn, result, sql)?;
    Ok((result, truncated))
}

/// Drains a query's row stream into a bounded [`QueryResult`], keeping at
/// most `max_rows` rows and capping each cell to `cell_cap` bytes. The first
/// bool is whether rows existed past the cap; the second is whether the
/// stream was abandoned with data still unread (extra result set — the row
/// cap breaking early always leaves data, so `truncated` implies it).
async fn run_query_capped(
    client: &mut TdsClient,
    sql: &str,
    params: &[Value],
    max_rows: u64,
    cell_cap: usize,
) -> Result<(QueryResult, bool, bool), TdsError> {
    let (rows, abandoned) = read_first_result_set(client, sql, params, |columns| {
        Ok(CappedRows {
            columns: columns.map(column_infos).unwrap_or_default(),
            rows: Vec::new(),
            max_rows,
            cell_cap,
            truncated: false,
        })
    })
    .await?;
    let result = QueryResult {
        columns: rows.columns,
        rows: rows.rows,
    };
    Ok((result, rows.truncated, abandoned))
}

/// [`run_query_capped`]'s sink: decodes and keeps at most `max_rows` rows,
/// each cell capped to `cell_cap` bytes.
struct CappedRows {
    columns: Vec<ColumnInfo>,
    rows: Vec<Vec<Value>>,
    max_rows: u64,
    cell_cap: usize,
    truncated: bool,
}

impl RowSink for CappedRows {
    fn row(&mut self, row: &Row) -> Result<ControlFlow<()>, TdsError> {
        // The cap+1'th row that reaches us proves there is more; stop before
        // decoding it so exactly `max_rows` rows are retained.
        if self.rows.len() as u64 >= self.max_rows {
            self.truncated = true;
            return Ok(ControlFlow::Break(()));
        }
        let values = decode_row(row)
            .into_iter()
            .map(|v| cap_value(v, self.cell_cap))
            .collect();
        self.rows.push(values);
        Ok(ControlFlow::Continue(()))
    }
}

/// Executes parameterized writes inside ONE transaction, committing only
/// when every statement affected exactly its `expected_rows` rows. Any SQL
/// error or count mismatch rolls the whole batch back; the error carries the
/// index of the failing statement (`None` for begin/commit failures, which
/// belong to no statement). This is the safety net for row edits: a WHERE
/// clause that unexpectedly matches more (or fewer) rows than the one being
/// edited must never commit.
pub async fn execute_all_checked(
    pool: &MssqlPool,
    statements: &[CheckedStatement],
) -> Result<(), (Option<usize>, DbError)> {
    let mut conn = pool.acquire().await.map_err(|e| (None, e))?;
    let begin = run_batch(conn.client(), "BEGIN TRAN").await;
    settle(&mut conn, begin, "BEGIN TRAN").map_err(|e| (None, e))?;
    for (index, statement) in statements.iter().enumerate() {
        let result = run_execute(conn.client(), &statement.sql, &statement.params).await;
        let affected = match result {
            Ok(counts) => statement_rows(&counts),
            Err(e) => {
                rollback_on(&mut conn, is_fatal(&e)).await;
                return Err((Some(index), query_error(&e, &statement.sql)));
            }
        };
        if affected != statement.expected_rows {
            rollback_on(&mut conn, false).await;
            return Err((
                Some(index),
                DbError::row_count_mismatch(affected, statement.expected_rows),
            ));
        }
    }
    match run_batch(conn.client(), "COMMIT TRAN").await {
        Ok(()) => Ok(()),
        Err(e) => {
            // Any commit failure — fatal or a server error like a doomed
            // transaction (3930) — may leave the session inside a transaction;
            // never return such a connection to the pool.
            conn.discard();
            Err((None, query_error(&e, "COMMIT TRAN")))
        }
    }
}

/// Best-effort rollback after a failure mid-transaction. When the failure was
/// fatal (stream state unknown) or the rollback itself fails, the connection
/// is discarded — an open transaction must never return to the pool.
async fn rollback_on(conn: &mut PooledConn, already_fatal: bool) {
    if already_fatal {
        conn.discard();
        return;
    }
    if run_batch(conn.client(), "IF @@TRANCOUNT > 0 ROLLBACK TRAN")
        .await
        .is_err()
    {
        conn.discard();
    }
}

/// A script transaction: one connection checked out of the pool for
/// the whole script, its statements tied together by a manual `BEGIN TRAN`.
/// Must be resolved via [`commit_tx`] or [`rollback_tx`]; a transaction
/// dropped unresolved discards its connection (never returning one with an
/// open transaction to the pool).
pub struct MssqlTx {
    conn: PooledConn,
    resolved: bool,
    /// `CURRENT_TRANSACTION_ID()` once `BEGIN TRAN` ran: [`rollback_tx`]
    /// compares it to tell this transaction from one a script opened after
    /// committing it.
    id: i64,
}

impl Drop for MssqlTx {
    fn drop(&mut self) {
        if !self.resolved {
            self.conn.discard();
        }
    }
}

/// Opens a script transaction on a dedicated pooled connection.
pub async fn begin_tx(pool: &MssqlPool) -> Result<MssqlTx, DbError> {
    const BEGIN: &str = "BEGIN TRAN; SELECT CURRENT_TRANSACTION_ID()";
    let mut conn = pool.acquire().await?;
    let begin = async {
        let row = conn.client().simple_query(BEGIN).await?.into_row().await?;
        Ok::<_, TdsError>(row.and_then(|row| row.get::<i64, _>(0)).unwrap_or(0))
    }
    .await;
    let id = settle(&mut conn, begin, BEGIN)?;
    Ok(MssqlTx {
        conn,
        resolved: false,
        id,
    })
}

/// Runs a non-row statement in the script transaction, returning affected
/// rows. Errors (instead of panicking) when an earlier fatal failure already
/// discarded the transaction's connection.
pub async fn execute_conn(tx: &mut MssqlTx, sql: &str) -> Result<u64, DbError> {
    execute_held(&mut tx.conn, sql).await
}

/// One script statement on a connection held across statements — a script
/// transaction's or a [`MssqlScriptConn`]'s.
async fn execute_held(conn: &mut PooledConn, sql: &str) -> Result<u64, DbError> {
    if conn.is_discarded() {
        return Err(lost_connection());
    }
    let result = run_statement(conn.client(), sql).await;
    settle(conn, result, sql)
}

/// [`execute_conn`] with bound parameters — the write path for a batch of
/// imported rows, which must run inside the script transaction and
/// must not interpolate its values into the SQL. Reports the statement's own
/// rows ([`statement_rows`]), not its triggers': the import holds each batch
/// to the rows it carried.
pub async fn execute_with_conn(
    tx: &mut MssqlTx,
    sql: &str,
    params: &[Value],
) -> Result<u64, DbError> {
    if tx.conn.is_discarded() {
        return Err(lost_connection());
    }
    let result = run_execute(tx.conn.client(), sql, params).await;
    settle(&mut tx.conn, result, sql).map(|counts| statement_rows(&counts))
}

/// Commits a script transaction — its statements all take effect. A failed
/// commit discards the connection (nothing was committed; the server rolls
/// the transaction back with the session). A transaction whose connection was
/// already discarded by an earlier fatal failure cannot commit and errors.
pub async fn commit_tx(mut tx: MssqlTx) -> Result<(), DbError> {
    if tx.conn.is_discarded() {
        tx.resolved = true;
        return Err(lost_connection());
    }
    let commit = run_batch(tx.conn.client(), "COMMIT TRAN").await;
    let result = settle(&mut tx.conn, commit, "COMMIT TRAN");
    match result {
        Ok(()) => {
            tx.resolved = true;
            Ok(())
        }
        Err(e) => {
            // Explicit: whether or not the failure was fatal, this connection
            // must not carry a half-dead transaction back to the pool.
            tx.conn.discard();
            tx.resolved = true;
            Err(e)
        }
    }
}

/// Rolls a script transaction back, and says whether what it rolled back was
/// still hubro's transaction. A statement can end it from inside the batch:
/// measured, `INSERT … COMMIT` (or an `EXEC`'d procedure that commits) fails
/// with error 266 after its `COMMIT` took effect, and `… COMMIT BEGIN TRAN`
/// raises nothing and leaves a new transaction in its place.
///
/// Best-effort: a rollback failure discards the connection, which rolls the
/// transaction back server-side anyway. On a connection an earlier fatal
/// failure already discarded this is a no-op — the server rolls the abandoned
/// transaction back when the dead connection goes away (the script runner
/// rolls back unconditionally after any statement error, so this path must
/// not panic).
pub async fn rollback_tx(mut tx: MssqlTx) -> RolledBack {
    if tx.conn.is_discarded() {
        tx.resolved = true;
        // shortcut: assumes the dead session still held hubro's transaction;
        // whether a script had ended it can no longer be asked.
        return RolledBack::Own;
    }
    // A plain batch, as any statement that changes `@@TRANCOUNT` must be.
    let rollback = format!(
        "DECLARE @found int = CASE WHEN @@TRANCOUNT = 0 THEN 0 \
         WHEN CURRENT_TRANSACTION_ID() = {} THEN 1 ELSE 2 END; \
         IF @@TRANCOUNT > 0 ROLLBACK TRAN; SELECT @found",
        tx.id
    );
    let found = async {
        let row = tx
            .conn
            .client()
            .simple_query(&rollback)
            .await?
            .into_row()
            .await?;
        Ok::<_, TdsError>(row.and_then(|row| row.get::<i32, _>(0)))
    }
    .await;
    match found {
        Ok(found) => {
            tx.resolved = true;
            match found {
                Some(0) => RolledBack::Nothing,
                Some(2) => RolledBack::Another,
                _ => RolledBack::Own,
            }
        }
        // The Drop impl discards the connection.
        Err(_) => RolledBack::Own,
    }
}

/// One pooled connection held for a whole script that runs outside hubro's
/// transaction — one that manages its own (`BEGIN TRAN` … `COMMIT`), or holds
/// a statement SQL Server refuses inside a user transaction. Holding it is
/// what keeps the script's own transaction on the connection that opened it.
///
/// Ended by [`end_script_conn`], which rolls back whatever transaction the
/// script left open. The registry's
/// [`ScriptConn`](crate::db::registry::ScriptConn) discards a connection that
/// was not ended — a cancelled run — so none goes back to the pool with a
/// transaction still open on it.
pub struct MssqlScriptConn {
    conn: PooledConn,
}

impl MssqlScriptConn {
    /// Drops the connection instead of returning it to the pool. The server
    /// rolls back any transaction left open on it when the session goes.
    pub fn discard(&mut self) {
        self.conn.discard();
    }
}

/// Checks a connection out for one script — see [`MssqlScriptConn`].
pub async fn acquire_script_conn(pool: &MssqlPool) -> Result<MssqlScriptConn, DbError> {
    Ok(MssqlScriptConn {
        conn: pool.acquire().await?,
    })
}

/// [`execute_conn`] on a script's held connection.
pub async fn execute_script_conn(conn: &mut MssqlScriptConn, sql: &str) -> Result<u64, DbError> {
    execute_held(&mut conn.conn, sql).await
}

/// [`query_capped_conn`] on a script's held connection.
pub async fn query_capped_script_conn(
    conn: &mut MssqlScriptConn,
    sql: &str,
    max_rows: u64,
    cell_cap: usize,
) -> Result<(QueryResult, bool), DbError> {
    query_capped_held(&mut conn.conn, sql, max_rows, cell_cap).await
}

/// Rolls back a transaction the script left open and answers one
/// question in the same batch: was there one. `@@TRANCOUNT` is the session's
/// own count, so it covers every nesting level and a transaction an error has
/// made uncommittable (`XACT_STATE() = -1`), which only a rollback can end.
///
/// A plain batch rather than `sp_executesql`, like every statement that
/// changes `@@TRANCOUNT`: inside the RPC a `ROLLBACK` that ends a transaction
/// opened outside it fails with error 266.
const END_SCRIPT: &str =
    "IF @@TRANCOUNT > 0 BEGIN ROLLBACK TRAN; SELECT CAST(1 AS int) END ELSE SELECT CAST(0 AS int)";

/// Ends a script's hold on its connection: rolls back any transaction the
/// script left open and reports whether there was one. A failure discards the
/// connection, since its transaction state is then unknown; the error is the
/// caller's to report.
pub async fn end_script_conn(conn: &mut MssqlScriptConn) -> Result<bool, DbError> {
    let held = &mut conn.conn;
    if held.is_discarded() {
        return Err(lost_connection());
    }
    let ended = async {
        let row = held
            .client()
            .simple_query(END_SCRIPT)
            .await?
            .into_row()
            .await?;
        Ok::<_, TdsError>(row.and_then(|row| row.get::<i32, _>(0)) == Some(1))
    }
    .await;
    let ended = settle(held, ended, END_SCRIPT);
    if ended.is_err() {
        held.discard();
    }
    ended
}

/// A script's last statement, when it returns rows, and the end of the
/// script's hold on its connection — see
/// [`ScriptConn::finish_with_read`](crate::db::registry::ScriptConn::finish_with_read).
///
/// Unless [`final_read_may_close`] says the batch can neither touch the
/// transaction nor write, the read is followed by the ordinary end, which
/// first reads and discards whatever of the response is left — a batch led by
/// a `SELECT` can carry `COMMIT`, `BEGIN TRAN` or an `INSERT … OUTPUT`, and
/// every one of them only finishes once its response has been read.
///
/// Otherwise `@@TRANCOUNT` is read *before* the read. A read the row cap cut
/// short, or one that left a second result set unread, is then followed by
/// nothing: tiberius would first drain the rest of the response — measured,
/// 1.55 s for a capped `SELECT` over 3M rows on localhost, where discarding
/// the connection takes 100 ms — so the connection is discarded instead, as a
/// capped read's always was, and the server rolls back whatever was open on
/// it. The cost is a reconnect (a TLS login; over an SSH tunnel, a new
/// channel) the next time the pool needs that connection.
pub async fn finish_script_conn_with_read(
    conn: &mut MssqlScriptConn,
    sql: &str,
    max_rows: u64,
    cell_cap: usize,
) -> crate::db::registry::FinishedRead {
    use crate::db::registry::FinishedRead;
    if !final_read_may_close(sql, crate::db::sql::Dialect::SqlServer) {
        let read = query_capped_held(&mut conn.conn, sql, max_rows, cell_cap).await;
        let ended = end_script_conn(conn).await;
        let reusable = ended.is_ok();
        return FinishedRead {
            read,
            ended,
            reusable,
        };
    }
    let held = &mut conn.conn;
    if held.is_discarded() {
        return FinishedRead {
            read: Err(lost_connection()),
            ended: Err(lost_connection()),
            reusable: false,
        };
    }
    let before = async {
        let row = held
            .client()
            .simple_query("SELECT @@TRANCOUNT")
            .await?
            .into_row()
            .await?;
        Ok::<_, TdsError>(row.and_then(|row| row.get::<i32, _>(0)).unwrap_or(0) > 0)
    }
    .await;
    let open = match settle(held, before, "SELECT @@TRANCOUNT") {
        Ok(open) => open,
        Err(error) => {
            held.discard();
            // Not the statement's own error: it never ran.
            let error = DbError::Query(format!(
                "the statement was not run: checking the connection's transaction before it \
                 failed: {error}"
            ));
            return FinishedRead {
                read: Err(error.clone()),
                ended: Err(error),
                reusable: false,
            };
        }
    };
    let read = run_query_capped(held.client(), sql, &[], max_rows, cell_cap).await;
    match settle(held, read, sql) {
        Ok((result, truncated, abandoned)) if truncated || abandoned => {
            held.discard();
            FinishedRead {
                read: Ok((result, truncated)),
                ended: Ok(open),
                reusable: false,
            }
        }
        read => {
            let read = read.map(|(result, truncated, _)| (result, truncated));
            let ended = end_script_conn(conn).await;
            let reusable = ended.is_ok();
            FinishedRead {
                read,
                ended,
                reusable,
            }
        }
    }
}

/// Streams a query to `out` in the given format, writing each row
/// incrementally — peak memory is one decoded row plus the writer's buffer.
/// Returns the number of data rows written. TDS sends result-set metadata
/// even for zero rows, so an empty result still gets its header (CSV) /
/// empty array (JSON) without a separate describe round-trip.
pub async fn export(
    pool: &MssqlPool,
    sql: &str,
    params: &[Value],
    format: ExportFormat,
    out: &mut impl Write,
) -> Result<u64, DbError> {
    let mut conn = pool.acquire().await?;
    // The stream borrows the client, so driver errors are collected and
    // settled after the stream is dropped.
    let outcome = async {
        let (mut rows, abandoned) = read_first_result_set(conn.client(), sql, params, |columns| {
            let columns = columns
                .map(|columns| columns.iter().map(|c| c.name().to_string()).collect())
                .unwrap_or_default();
            let mut sink = ExportSink::new(format, columns);
            sink.begin(out).map_err(io_to_tds)?;
            Ok(ExportRows {
                sink,
                out,
                written: 0,
            })
        })
        .await?;
        rows.sink.end(rows.out).map_err(io_to_tds)?;
        Ok((rows.written, abandoned))
    }
    .await;
    let (written, abandoned) = settle(&mut conn, outcome, sql)?;
    // An abandoned second result set must not ride back to the pool.
    if abandoned {
        conn.discard();
    }
    Ok(written)
}

/// [`export`]'s sink: writes each row through as it arrives.
struct ExportRows<'w, W> {
    sink: ExportSink,
    out: &'w mut W,
    written: u64,
}

impl<W: Write> RowSink for ExportRows<'_, W> {
    fn row(&mut self, row: &Row) -> Result<ControlFlow<()>, TdsError> {
        let values = decode_row(row);
        self.sink.write_row(&values, self.out).map_err(io_to_tds)?;
        self.written += 1;
        Ok(ControlFlow::Continue(()))
    }
}

/// Smuggles a writer I/O failure through the driver error type so the export
/// sink and the result loop it runs in share a single error channel;
/// unwrapped by [`query_error`] into the same message [`export_io_err`] would
/// produce.
fn io_to_tds(err: std::io::Error) -> TdsError {
    let db_err = export_io_err(err);
    TdsError::Io {
        kind: std::io::ErrorKind::Other,
        message: db_err.message().to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::sqlserver::mssql_url_via_local_port;

    #[test]
    fn tunneled_config_keeps_the_logical_host_for_tls() {
        // A tunneled connect parses the rewritten URL (dialing
        // 127.0.0.1:<forwarded>) but overrides the config host with the
        // logical one — tiberius takes the TLS server name from the config, so
        // encrypt=on validates the real certificate through the tunnel.
        let rewritten =
            mssql_url_via_local_port("mssql://sa@db.example.com:1433/app?encrypt=on", 40123)
                .unwrap();
        let parts = parse_mssql_url(&rewritten).unwrap();
        assert_eq!(parts.host, "127.0.0.1");
        assert_eq!(parts.port, 40123);
        let config = parts.into_config(&MssqlAuth::Password, Some("db.example.com"));
        assert_eq!(config.get_addr(), "db.example.com:40123");
        // Without an override the config host is the URL's.
        let direct = parse_mssql_url("mssql://sa@db.example.com:1433/app")
            .unwrap()
            .into_config(&MssqlAuth::Password, None);
        assert_eq!(direct.get_addr(), "db.example.com:1433");
    }

    #[test]
    fn mssql_auth_debug_never_prints_the_token() {
        let auth = MssqlAuth::AadToken("SECRET_TOKEN".to_string());
        let rendered = format!("{auth:?}");
        assert!(!rendered.contains("SECRET_TOKEN"), "{rendered}");
        assert!(rendered.contains("AadToken"));
    }

    #[test]
    fn parse_url_extracts_all_parts() {
        let parts = parse_mssql_url(
            "mssql://sa:p%40ss@db.example.com:14330/app?encrypt=off&trustServerCertificate=true",
        )
        .unwrap();
        assert_eq!(
            parts,
            MssqlUrlParts {
                host: "db.example.com".into(),
                port: 14330,
                user: "sa".into(),
                password: "p@ss".into(),
                database: Some("app".into()),
                encryption: EncryptionLevel::Off,
                trust_server_certificate: true,
            }
        );
    }

    #[test]
    fn parse_url_defaults_port_database_and_encryption() {
        let parts = parse_mssql_url("mssql://sa@host").unwrap();
        assert_eq!(parts.port, 1433);
        assert_eq!(parts.database, None);
        assert_eq!(parts.password, "");
        // With TLS compiled in, the default is encrypt-and-fail-if-not.
        assert_eq!(parts.encryption, EncryptionLevel::Required);
        assert!(!parts.trust_server_certificate);
    }

    #[test]
    fn parse_url_maps_encrypt_values() {
        let level = |v: &str| {
            parse_mssql_url(&format!("mssql://u@h?encrypt={v}"))
                .unwrap()
                .encryption
        };
        assert_eq!(level("on"), EncryptionLevel::Required);
        assert_eq!(level("required"), EncryptionLevel::Required);
        assert_eq!(level("off"), EncryptionLevel::Off);
        assert_eq!(level("plaintext"), EncryptionLevel::NotSupported);
        assert!(parse_mssql_url("mssql://u@h?encrypt=maybe").is_err());
    }

    #[test]
    fn parse_url_rejects_unknown_params_and_wrong_scheme() {
        // Unknown params are rejected rather than ignored, so a typo can't
        // silently weaken TLS settings.
        assert!(parse_mssql_url("mssql://u@h?sslmode=require").is_err());
        assert!(parse_mssql_url("mssql://u@h?trustServerCertificate=maybe").is_err());
        assert!(parse_mssql_url("postgres://u@h/db").is_err());
    }

    #[test]
    fn connect_errors_are_categorized() {
        let auth = TdsError::Io {
            kind: std::io::ErrorKind::Other,
            message: "Login failed for user 'sa'.".into(),
        };
        assert!(friendly_connect_error(&auth).starts_with("authentication failed"));
        let net = TdsError::Io {
            kind: std::io::ErrorKind::ConnectionRefused,
            message: "Connection refused (os error 61)".into(),
        };
        assert!(friendly_connect_error(&net).starts_with("network error"));
        let tls = TdsError::Tls("handshake failed".into());
        assert!(friendly_connect_error(&tls).starts_with("TLS error"));
        // A non-SQL-Server server answering the TDS handshake (e.g. Postgres
        // on 5432) fails packet parsing — a wrong-server problem, not a
        // protocol bug. The reverse of the Postgres-side wrong-server hint.
        let not_tds = TdsError::Protocol("header: invalid packet type: 69".into());
        let friendly = friendly_connect_error(&not_tds);
        assert!(friendly.starts_with("the server doesn't appear to be SQL Server"));
        assert!(!friendly.starts_with("TLS error"));
        // Some non-TDS servers (Postgres among them) just hang up on the
        // prelogin instead of answering — an EOF at connect is the same
        // wrong-server story, not a generic network error.
        let hangup = TdsError::Io {
            kind: std::io::ErrorKind::UnexpectedEof,
            message: "failed to fill whole buffer".into(),
        };
        let friendly = friendly_connect_error(&hangup);
        assert!(friendly.starts_with("the server doesn't appear to be SQL Server"));
        // tiberius 0.13's handshake timeout, verbatim: a stalled server, not a
        // TLS failure, and none of the driver-configuration advice reaches
        // the user.
        let stalled = TdsError::Io {
            kind: std::io::ErrorKind::TimedOut,
            message: "the connection handshake (prelogin, TLS negotiation and login) did not \
                      complete within 15s; the server accepted the TCP connection but did not \
                      finish the handshake in time (it may have stalled, or be responding too \
                      slowly for the configured bound). Enable `tracing` at DEBUG to see which \
                      stage was last reached, or change the bound with \
                      `Config::handshake_timeout`."
                .into(),
        };
        let friendly = friendly_connect_error(&stalled);
        assert!(friendly.starts_with("network error"), "{friendly}");
        assert!(!friendly.contains("tracing") && !friendly.contains("Config::"));
        // A TCP-level timeout keeps the OS's wording.
        let tcp = TdsError::Io {
            kind: std::io::ErrorKind::TimedOut,
            message: "Connection timed out (os error 110)".into(),
        };
        let friendly = friendly_connect_error(&tcp);
        assert!(friendly.starts_with("network error"), "{friendly}");
        assert!(
            friendly.ends_with("Connection timed out (os error 110)"),
            "{friendly}"
        );
    }

    /// Error 4060's text as SQL Server 2022 sends it, measured — and the
    /// reason the database arm must come before the login arm.
    const CANNOT_OPEN_TEXT: &str =
        "Cannot open database \"no_such_db\" requested by the login. The login failed.";

    #[test]
    fn only_login_failed_rejects_the_credentials() {
        // The connect flow prompts for a new password on CredentialsRejected,
        // and an unticked different answer deletes the stored one, so the
        // variant is a promise that the *password* was refused. Every
        // code below was measured against SQL Server 2022.
        let rejected = |code| connect_error_from(code, "m".into()).credentials_rejected();
        assert!(
            rejected(Some(LOGIN_FAILED)),
            "a wrong password must raise the prompt"
        );
        for (code, what) in [
            (
                Some(CANNOT_OPEN_DATABASE),
                "a missing, offline, restoring or forbidden database",
            ),
            (
                Some(18470),
                "a disabled login, whose password is still right",
            ),
            (Some(1205), "a deadlock victim"),
            (None, "a failure with no server code at all"),
        ] {
            assert!(
                !rejected(code),
                "{what} ({code:?}) would delete a working password"
            );
        }
        // The text never decides, whichever way it points: a login-failed
        // message without the code is reported and keeps the password…
        let text_only = TdsError::Io {
            kind: std::io::ErrorKind::Other,
            message: "Login failed for user 'sa'.".into(),
        };
        assert_eq!(
            connect_error(&text_only),
            DbError::Connect(friendly_connect_error(&text_only))
        );
        // …and the message survives the classification unchanged.
        assert_eq!(
            connect_error_from(Some(LOGIN_FAILED), "the words".into()),
            DbError::CredentialsRejected("the words".into())
        );
    }

    #[test]
    fn a_database_that_cannot_be_opened_is_not_called_an_authentication_failure() {
        // Words only — the decision is the code's — but words that say
        // "authentication failed" for a missing database send the user off to
        // fix a password that works. The arm order is what decides it, since
        // 4060's message contains "login failed" too.
        let err = TdsError::Io {
            kind: std::io::ErrorKind::Other,
            message: CANNOT_OPEN_TEXT.into(),
        };
        let friendly = friendly_connect_error(&err);
        assert!(
            friendly.starts_with("cannot open the database"),
            "{friendly}"
        );
        assert!(!friendly.contains("authentication failed"), "{friendly}");
        assert!(friendly.ends_with(CANNOT_OPEN_TEXT), "{friendly}");
    }

    #[test]
    fn statements_are_not_bounded_by_a_command_timeout() {
        // tiberius 0.13 defaults to a 30s bound per round-trip; a query that
        // runs longer than that is the user's business, not a driver error.
        let parts = || parse_mssql_url("mssql://sa@db.example.com:1433/app").unwrap();
        let token = MssqlAuth::AadToken("t".to_string());
        for config in [
            parts().into_config(&MssqlAuth::Password, None),
            parts().into_config(&token, Some("db.example.com")),
        ] {
            assert_eq!(config.get_command_timeout(), None);
        }
    }

    #[test]
    fn fatal_errors_are_distinguished_from_sql_errors() {
        // Driver/stream-level failures poison the connection…
        assert!(is_fatal(&TdsError::Protocol("oops".into())));
        assert!(is_fatal(&TdsError::Io {
            kind: std::io::ErrorKind::BrokenPipe,
            message: "broken pipe".into(),
        }));
        // …but Error::Server (an ordinary SQL error token) does not. Not
        // constructible outside tiberius, so assert via the classifier's
        // definition rather than a live value: everything except Server is
        // fatal, which the two probes above pin down.
    }

    /// A transaction whose connection an earlier fatal failure discarded —
    /// the state the script runner hits when a driver-level error happens
    /// mid-script and then unconditionally rolls back.
    fn discarded_tx() -> MssqlTx {
        let inner = Arc::new(PoolInner {
            config: Config::new(),
            addr: ("localhost".to_string(), 1433),
            permits: Arc::new(Semaphore::new(1)),
            idle: Mutex::new(Vec::new()),
            closed: AtomicBool::new(false),
        });
        let permit = Arc::clone(&inner.permits)
            .try_acquire_owned()
            .expect("fresh semaphore has a permit");
        MssqlTx {
            conn: PooledConn {
                client: None,
                pool: inner,
                _permit: permit,
            },
            resolved: false,
            id: 0,
        }
    }

    #[tokio::test]
    async fn rollback_of_a_discarded_tx_is_a_noop_not_a_panic() {
        // The server rolls the abandoned transaction back when the dead
        // connection goes away; client-side there is nothing left to do.
        assert_eq!(rollback_tx(discarded_tx()).await, RolledBack::Own);
    }

    #[tokio::test]
    async fn commit_of_a_discarded_tx_errors_instead_of_panicking() {
        let err = commit_tx(discarded_tx()).await.unwrap_err();
        assert!(err.to_string().contains("connection was lost"));
    }

    #[tokio::test]
    async fn statements_on_a_discarded_tx_error_instead_of_panicking() {
        let mut tx = discarded_tx();
        let err = execute_conn(&mut tx, "SELECT 1").await.unwrap_err();
        assert!(err.to_string().contains("connection was lost"));
        let err = query_capped_conn(&mut tx, "SELECT 1", 10, 100)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("connection was lost"));
        rollback_tx(tx).await;
    }

    #[test]
    fn a_statement_is_held_to_its_own_count_not_its_triggers() {
        // The count sequences SQL Server 2022 sent, in arrival order, for one
        // statement through `sp_executesql`. Trigger counts come first; the
        // statement's own is last.
        let cases: &[(&str, &[u64], u64)] = &[
            ("no trigger", &[1], 1),
            ("an after trigger writing an audit row", &[1, 1], 1),
            (
                "an after trigger writing more rows than the statement",
                &[2, 5, 1],
                1,
            ),
            ("a nested trigger", &[1, 1, 1, 1], 1),
            // Count-less tokens inside triggers arrive as zeros, ahead of the
            // statement's own count.
            (
                "triggers mixing counts and count-less tokens",
                &[7, 1, 0, 0, 0, 2, 0, 1],
                1,
            ),
            ("an instead-of trigger writing other rows", &[5, 1], 1),
            // The summed count read this as the one row the edit expected.
            (
                "a statement-level trigger on a statement matching nothing",
                &[1, 0],
                0,
            ),
            // Under NOCOUNT ON: no count is no rows, never a pass.
            ("no counts at all", &[], 0),
        ];
        for (case, counts, expected) in cases {
            assert_eq!(statement_rows(counts), *expected, "{case}");
        }
    }

    #[test]
    fn every_session_reports_row_counts() {
        // `statement_rows` is only sound while the session sends counts, and
        // a server's `user options` can turn them off for new sessions.
        let options: Vec<String> = SESSION_SETUP
            .split(';')
            .map(|option| option.split_whitespace().collect::<Vec<_>>().join(" "))
            .collect();
        assert!(
            options.iter().any(|option| option == "SET NOCOUNT OFF"),
            "{options:?}"
        );
    }

    #[tokio::test]
    async fn a_connect_that_gets_no_answer_is_abandoned_at_its_bound() {
        // A listener that is never accepted from: the kernel completes the TCP
        // handshake, and then nothing answers the prelogin — the stage
        // tiberius's own 15s bound covers, which is why the outer limit here is
        // shorter than that: only the connect's own bound can end this in time.
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let addr = (
            "127.0.0.1".to_string(),
            listener.local_addr().unwrap().port(),
        );
        let mut config = Config::new();
        config.host(&addr.0);
        config.port(addr.1);
        let started = Instant::now();
        let result = tokio::time::timeout(
            Duration::from_secs(10),
            connect_client(&config, &addr, Duration::from_millis(300)),
        )
        .await
        .expect("the connect must give up at its own bound, not wait for tiberius's");
        let Err(DbError::Connect(message)) = result else {
            panic!("expected a connect error, got {result:?}");
        };
        assert!(
            message.starts_with("network error") && message.contains("within 300ms"),
            "{message}"
        );
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn an_idle_connection_is_checked_unless_just_used_or_idle_past_the_timeout() {
        assert_eq!(idle_reuse(Duration::ZERO), IdleReuse::Reuse);
        assert_eq!(idle_reuse(CHECK_AFTER), IdleReuse::Check);
        assert_eq!(idle_reuse(IDLE_TIMEOUT), IdleReuse::Check);
        assert_eq!(
            idle_reuse(IDLE_TIMEOUT + Duration::from_secs(1)),
            IdleReuse::Discard
        );
    }

    /// A TCP forwarder that can be told to go silent on the connections it
    /// already carries, as a NAT or firewall that dropped them would: their
    /// sockets stay open and nothing more arrives on either side. Connections
    /// made after that are forwarded normally.
    async fn freezable_proxy(target: (String, u16)) -> (u16, Arc<std::sync::atomic::AtomicUsize>) {
        use std::sync::atomic::AtomicUsize;
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let port = listener.local_addr().unwrap().port();
        let epoch = Arc::new(AtomicUsize::new(0));
        let accept_epoch = Arc::clone(&epoch);
        tokio::spawn(async move {
            while let Ok((client, _)) = listener.accept().await {
                let server = TcpStream::connect((target.0.as_str(), target.1))
                    .await
                    .unwrap();
                let born = accept_epoch.load(Ordering::SeqCst);
                let (client_read, client_write) = client.into_split();
                let (server_read, server_write) = server.into_split();
                for (mut from, mut to) in [
                    (
                        Box::new(client_read) as Box<dyn tokio::io::AsyncRead + Send + Unpin>,
                        Box::new(server_write) as Box<dyn tokio::io::AsyncWrite + Send + Unpin>,
                    ),
                    (Box::new(server_read), Box::new(client_write)),
                ] {
                    let epoch = Arc::clone(&accept_epoch);
                    tokio::spawn(async move {
                        let mut buf = vec![0u8; 16 * 1024];
                        loop {
                            let n = match from.read(&mut buf).await {
                                Ok(0) | Err(_) => return,
                                Ok(n) => n,
                            };
                            if epoch.load(Ordering::SeqCst) != born {
                                // Frozen: hold both sockets open, forward nothing.
                                std::future::pending::<()>().await;
                            }
                            if to.write_all(&buf[..n]).await.is_err() {
                                return;
                            }
                        }
                    });
                }
            }
        });
        (port, epoch)
    }

    #[tokio::test]
    async fn an_idle_connection_that_never_answers_is_replaced_not_waited_on() {
        let Ok(url) = std::env::var("HUBRO_MSSQL_TEST_URL") else {
            eprintln!("skipping: HUBRO_MSSQL_TEST_URL not set");
            return;
        };
        let parts = parse_mssql_url(&url).unwrap();
        let target = (parts.host.clone(), parts.port);
        let config = parts.into_config(&MssqlAuth::Password, None);
        let (port, epoch) = freezable_proxy(target).await;
        let pool = MssqlPool::new(config, ("127.0.0.1".to_string(), port));
        // Three connections, used and returned, and idle long enough to be
        // checked before reuse.
        {
            let held = [
                pool.acquire().await.unwrap(),
                pool.acquire().await.unwrap(),
                pool.acquire().await.unwrap(),
            ];
            drop(held);
        }
        for idle in pool.inner.idle.lock().unwrap().iter_mut() {
            idle.since -= CHECK_AFTER;
        }
        assert_eq!(pool.inner.idle.lock().unwrap().len(), 3);

        epoch.fetch_add(1, Ordering::SeqCst);
        let started = Instant::now();
        // Without the check the query goes to a silent connection and waits
        // for good (statements have no timeout); with a check that has no
        // bound, so does the check.
        let result = tokio::time::timeout(
            PING_TIMEOUT * 3 + Duration::from_secs(15),
            query_with(&pool, "SELECT 2", &[]),
        )
        .await
        .expect("a silent idle connection must be given up on, not waited on");
        assert_eq!(result.unwrap().rows, vec![vec![Value::Integer(2)]]);
        let waited = started.elapsed();
        assert!(waited >= PING_TIMEOUT, "{waited:?}");
        // One silent check is the verdict on all of them: the other two are
        // not each waited out in turn.
        assert!(waited < PING_TIMEOUT * 2, "{waited:?}");
        pool.close().await;
    }
}
