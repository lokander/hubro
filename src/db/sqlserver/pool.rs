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
//! tiberius has no transaction API; `BEGIN TRAN`/`COMMIT`/`ROLLBACK` run as
//! raw batches on one checked-out connection ([`MssqlTx`]), which the server
//! ties together via the session's transaction descriptor.

use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use futures_util::TryStreamExt as _;
use tiberius::error::Error as TdsError;
use tiberius::{AuthMethod, Client, Config, EncryptionLevel, Query, QueryItem};
use tokio::net::TcpStream;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio_util::compat::{Compat, TokioAsyncWriteCompatExt as _};

use super::{column_infos, decode_row};
use crate::db::error::DbError;
use crate::db::export::{export_io_err, ExportFormat, ExportSink};
use crate::db::staged::CheckedStatement;
use crate::db::value::{cap_value, QueryResult, Value};

/// Matches PgPool's `max_connections(4)`: enough for the grid, introspection,
/// and a script transaction to run concurrently without deadlocking.
const MAX_CONNECTIONS: usize = 4;

/// Session options every pooled connection runs right after login. tiberius
/// logs in with the ODBC option flag, whose server-side defaults already
/// include `QUOTED_IDENTIFIER ON`, but nothing in the login *guarantees* it —
/// and the app's identifier quoting is ANSI double quotes everywhere, which
/// only parses with QUOTED_IDENTIFIER ON. Set it (and the ANSI family SQL
/// Server's own drivers set) explicitly so the session never depends on
/// server or database defaults. TEXTSIZE lifts the legacy cap on
/// text/ntext/image reads.
const SESSION_SETUP: &str = "SET QUOTED_IDENTIFIER ON; \
     SET ANSI_NULLS ON; \
     SET ANSI_PADDING ON; \
     SET ANSI_WARNINGS ON; \
     SET CONCAT_NULL_YIELDS_NULL ON; \
     SET TEXTSIZE 2147483647;";

type TdsClient = Client<Compat<TcpStream>>;

/// How a SQL Server connect authenticates (FRE-58). [`open_mssql`] always uses
/// `Password`; the Entra flow passes `AadToken` to [`open_mssql_with`].
#[derive(Clone)]
pub enum MssqlAuth {
    /// SQL Server authentication with the URL's user and (possibly spliced-in)
    /// password — the FRE-57 behavior.
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
    idle: Mutex<Vec<TdsClient>>,
    closed: AtomicBool,
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
                    .push(client);
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
    /// available (up to [`MAX_CONNECTIONS`] total; further callers wait).
    async fn acquire(&self) -> Result<PooledConn, DbError> {
        let permit = Arc::clone(&self.inner.permits)
            .acquire_owned()
            .await
            .map_err(|_| DbError::Query("the connection is closed".into()))?;
        let idle = self.inner.idle.lock().expect("pool lock poisoned").pop();
        let client = match idle {
            Some(client) => client,
            None => connect_client(&self.inner.config, &self.inner.addr).await?,
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
        let clients: Vec<TdsClient> = {
            let mut idle = self.inner.idle.lock().expect("pool lock poisoned");
            idle.drain(..).collect()
        };
        for client in clients {
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

/// [`open_mssql`] with explicit auth and an optional TLS host override
/// (FRE-58). `tls_host` is the server's logical hostname for an SSH-tunneled
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
                DbError::Connect(friendly_connect_error(&e))
            })?;
        // conn drops here and seeds the idle list.
    }
    Ok(pool)
}

/// Opens a TCP connection to `addr` and performs the TDS login, following at
/// most one Azure-style routing redirect, then applies [`SESSION_SETUP`].
async fn connect_client(config: &Config, addr: &(String, u16)) -> Result<TdsClient, DbError> {
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
    .map_err(|e| DbError::Connect(friendly_connect_error(&e)))?;
    match run_batch(&mut client, SESSION_SETUP).await {
        Ok(()) => Ok(client),
        Err(e) => Err(DbError::Connect(friendly_connect_error(&e))),
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

/// Categorizes common failure modes so the connections screen reads well:
/// auth, network/DNS, TLS, wrong-server.
fn friendly_connect_error(err: &TdsError) -> String {
    let msg = err.to_string();
    let lower = msg.to_lowercase();
    if lower.contains("login failed") {
        format!("authentication failed — {msg}")
    } else if lower.contains("cannot open database") {
        format!("unknown database — {msg}")
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
        // FRE-51 hint.
        format!("the server doesn't appear to be SQL Server — check the host and port — {msg}")
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
    let mut query = Query::new(sql.to_string());
    bind_params(&mut query, params);
    let mut stream = query.query(client).await?;
    let columns = match stream.columns().await? {
        Some(columns) => column_infos(columns),
        None => Vec::new(),
    };
    let mut rows = Vec::new();
    let mut result_sets = 0usize;
    let mut abandoned = false;
    while let Some(item) = stream.try_next().await? {
        match item {
            QueryItem::Metadata(_) => {
                result_sets += 1;
                // Only the first result set is kept — mirrors the sqlx
                // backends, where one statement yields one result.
                if result_sets > 1 {
                    abandoned = true;
                    break;
                }
            }
            QueryItem::Row(row) => rows.push(decode_row(&row)),
        }
    }
    Ok((QueryResult { columns, rows }, abandoned))
}

/// Runs a parameterized non-row statement via the driver's RPC path,
/// returning the summed affected-row count.
async fn run_execute(client: &mut TdsClient, sql: &str, params: &[Value]) -> Result<u64, TdsError> {
    let mut query = Query::new(sql.to_string());
    bind_params(&mut query, params);
    let result = query.execute(client).await?;
    Ok(result.total())
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
    let result = run_execute(conn.client(), sql, &[]).await;
    settle(&mut conn, result, sql)
}

/// Streams `sql`, decoding and retaining at most `max_rows` rows and capping
/// each cell to `cell_cap` bytes, so the free-form query path never scales
/// with table or value size (FRE-33). Returns the (bounded) result and
/// whether more rows existed beyond the cap.
///
/// A truncated read (or an abandoned second result set) leaves the rest of
/// the server's response mid-stream; tiberius would silently drain it before
/// the *next* query on that connection, which for a huge result would defer
/// the whole download to some innocent later query — so the connection is
/// discarded instead (one reconnect is far cheaper).
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
        conn.discard();
    }
    Ok((result, truncated))
}

/// [`query_capped`] against the connection held by a script transaction — the
/// read path for statements inside an atomically-wrapped script (FRE-38). No
/// bound params: scripts are raw text. Unlike the pool path, a truncated read
/// keeps the connection (discarding it would kill the open transaction); the
/// deferred drain of the abandoned rows is the price of atomicity.
pub async fn query_capped_conn(
    tx: &mut MssqlTx,
    sql: &str,
    max_rows: u64,
    cell_cap: usize,
) -> Result<(QueryResult, bool), DbError> {
    if tx.conn.is_discarded() {
        return Err(lost_connection());
    }
    let result = run_query_capped(tx.conn.client(), sql, &[], max_rows, cell_cap).await;
    let (result, truncated, _abandoned) = settle(&mut tx.conn, result, sql)?;
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
    let mut query = Query::new(sql.to_string());
    bind_params(&mut query, params);
    let mut stream = query.query(client).await?;
    let columns = match stream.columns().await? {
        Some(columns) => column_infos(columns),
        None => Vec::new(),
    };
    let mut rows: Vec<Vec<Value>> = Vec::new();
    let mut truncated = false;
    let mut abandoned = false;
    let mut result_sets = 0usize;
    while let Some(item) = stream.try_next().await? {
        match item {
            QueryItem::Metadata(_) => {
                result_sets += 1;
                if result_sets > 1 {
                    abandoned = true;
                    break;
                }
            }
            QueryItem::Row(row) => {
                // The cap+1'th row that reaches us proves there is more; stop
                // before decoding it so exactly `max_rows` rows are retained.
                if rows.len() as u64 >= max_rows {
                    truncated = true;
                    abandoned = true;
                    break;
                }
                let values = decode_row(&row)
                    .into_iter()
                    .map(|v| cap_value(v, cell_cap))
                    .collect();
                rows.push(values);
            }
        }
    }
    Ok((QueryResult { columns, rows }, truncated, abandoned))
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
            Ok(affected) => affected,
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

/// A script transaction (FRE-38): one connection checked out of the pool for
/// the whole script, its statements tied together by a manual `BEGIN TRAN`.
/// Must be resolved via [`commit_tx`] or [`rollback_tx`]; a transaction
/// dropped unresolved discards its connection (never returning one with an
/// open transaction to the pool).
pub struct MssqlTx {
    conn: PooledConn,
    resolved: bool,
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
    let mut conn = pool.acquire().await?;
    let begin = run_batch(conn.client(), "BEGIN TRAN").await;
    settle(&mut conn, begin, "BEGIN TRAN")?;
    Ok(MssqlTx {
        conn,
        resolved: false,
    })
}

/// Runs a non-row statement in the script transaction, returning affected
/// rows. Errors (instead of panicking) when an earlier fatal failure already
/// discarded the transaction's connection.
pub async fn execute_conn(tx: &mut MssqlTx, sql: &str) -> Result<u64, DbError> {
    if tx.conn.is_discarded() {
        return Err(lost_connection());
    }
    let result = run_execute(tx.conn.client(), sql, &[]).await;
    settle(&mut tx.conn, result, sql)
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

/// Rolls a script transaction back. Best-effort: a rollback failure discards
/// the connection, which rolls the transaction back server-side anyway. On a
/// connection an earlier fatal failure already discarded this is a no-op
/// success — the server rolls the abandoned transaction back when the dead
/// connection goes away (the script runner rolls back unconditionally after
/// any statement error, so this path must not panic).
pub async fn rollback_tx(mut tx: MssqlTx) {
    if tx.conn.is_discarded() {
        tx.resolved = true;
        return;
    }
    let rollback = run_batch(tx.conn.client(), "IF @@TRANCOUNT > 0 ROLLBACK TRAN").await;
    if rollback.is_ok() {
        tx.resolved = true;
    }
    // On error the Drop impl discards the connection.
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
    let mut query = Query::new(sql.to_string());
    bind_params(&mut query, params);
    // The stream borrows the client, so driver errors are collected and
    // settled after the stream is dropped.
    let mut written = 0u64;
    let mut abandoned = false;
    let outcome: Result<(), TdsError> = async {
        let mut stream = query.query(conn.client()).await?;
        let columns: Vec<String> = match stream.columns().await? {
            Some(columns) => columns.iter().map(|c| c.name().to_string()).collect(),
            None => Vec::new(),
        };
        let mut sink = ExportSink::new(format, columns);
        sink.begin(out).map_err(io_to_tds)?;
        let mut result_sets = 0usize;
        while let Some(item) = stream.try_next().await? {
            match item {
                QueryItem::Metadata(_) => {
                    result_sets += 1;
                    if result_sets > 1 {
                        abandoned = true;
                        break;
                    }
                }
                QueryItem::Row(row) => {
                    let values = decode_row(&row);
                    sink.write_row(&values, out).map_err(io_to_tds)?;
                    written += 1;
                }
            }
        }
        sink.end(out).map_err(io_to_tds)
    }
    .await;
    settle(&mut conn, outcome, sql)?;
    // An abandoned second result set must not ride back to the pool.
    if abandoned {
        conn.discard();
    }
    Ok(written)
}

/// Smuggles a writer I/O failure through the driver error type so the export
/// loop above has a single error channel; unwrapped by [`query_error`] into
/// the same message [`export_io_err`] would produce.
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
        // protocol bug. The reverse of the FRE-51 Postgres-side hint.
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
        }
    }

    #[tokio::test]
    async fn rollback_of_a_discarded_tx_is_a_noop_not_a_panic() {
        // The server rolls the abandoned transaction back when the dead
        // connection goes away; client-side there is nothing left to do.
        rollback_tx(discarded_tx()).await;
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
}
