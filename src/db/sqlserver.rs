//! SQL Server backend on tiberius: connecting and query execution.
//! Introspection here is a minimal stub (tables/views + columns from
//! INFORMATION_SCHEMA); full metadata parity (PKs, indexes, FKs, identity/
//! computed columns) lands with FRE-56.
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
use tiberius::{AuthMethod, Client, ColumnType, Config, EncryptionLevel, Query, QueryItem, Row};
use tokio::net::TcpStream;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio_util::compat::{Compat, TokioAsyncWriteCompatExt as _};

use super::error::DbError;
use super::export::{export_io_err, ExportFormat, ExportSink};
use super::schema::{ColumnMeta, Generated, TableKind, TableMeta};
use super::staged::CheckedStatement;
use super::value::{cap_value, ColumnInfo, QueryResult, Value};

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

/// Splices a password into an mssql URL (percent-encoding handled by the url
/// crate). Saved config stores URLs without passwords; this rebuilds the full
/// URL at connect time.
pub fn mssql_url_with_password(url: &str, password: &str) -> Result<String, DbError> {
    let mut parsed =
        url::Url::parse(url).map_err(|e| DbError::Connect(format!("invalid URL: {e}")))?;
    // set_password encodes most special characters but passes '%' through,
    // which would be mis-decoded on parse; encode it up front.
    let password = password.replace('%', "%25");
    parsed
        .set_password(Some(&password))
        .map_err(|_| DbError::Connect("this URL cannot carry a password".into()))?;
    Ok(parsed.into())
}

/// Canonicalizes an mssql URL into the stable form used as a saved-connection
/// locator and keyring account key, so the same server written different ways
/// maps to one entry and one stored secret. Validates the scheme, then:
///
/// - strips any password (never persisted),
/// - rewrites `sqlserver://` to `mssql://`,
/// - lowercases the host (DNS is case-insensitive; IP literals are unaffected),
/// - fills the default port `1433` when omitted, so `host` and `host:1433`
///   coincide.
///
/// Query params (e.g. `encrypt`) and the database path are left as-is.
pub fn normalize_mssql_url(url: &str) -> Result<String, DbError> {
    let mut parsed =
        url::Url::parse(url.trim()).map_err(|e| DbError::Connect(format!("invalid URL: {e}")))?;
    if parsed.scheme() != "mssql" && parsed.scheme() != "sqlserver" {
        return Err(DbError::Connect(format!(
            "expected an mssql:// URL, got {}://",
            parsed.scheme()
        )));
    }
    if parsed.scheme() == "sqlserver" {
        // Both are non-special schemes, so this never fails; ignore defensively.
        let _ = parsed.set_scheme("mssql");
    }
    let _ = parsed.set_password(None);
    if let Some(host) = parsed.host_str() {
        let lowered = host.to_ascii_lowercase();
        if lowered != host {
            parsed
                .set_host(Some(&lowered))
                .map_err(|e| DbError::Connect(format!("invalid host: {e}")))?;
        }
    }
    match parsed.port() {
        // 0 is not a usable port; same rule as the Postgres locator (FRE-42).
        Some(0) => return Err(DbError::Connect("port must be between 1 and 65535".into())),
        // mssql is a non-special scheme, so the url crate always serializes an
        // explicit port — the bare and `:1433` forms now serialize equal.
        None => {
            let _ = parsed.set_port(Some(1433));
        }
        Some(_) => {}
    }
    Ok(parsed.into())
}

/// The host and port an mssql URL points at (default port 1433) — with an SSH
/// tunnel this is the address the SSH server must reach.
pub fn mssql_url_target(url: &str) -> Result<(String, u16), DbError> {
    let parsed = url::Url::parse(url).map_err(|e| DbError::Connect(format!("invalid URL: {e}")))?;
    let host = parsed
        .host_str()
        .ok_or_else(|| DbError::Connect("URL has no host".into()))?
        // IPv6 hosts come back bracketed; the forward target wants the bare
        // address.
        .trim_matches(['[', ']'])
        .to_string();
    Ok((host, parsed.port().unwrap_or(1433)))
}

/// Rewrites a URL to connect through a forwarded local port; everything else
/// (user, database, query params) is kept. The saved URL stays the logical
/// one — this form is only ever used for the actual connect.
pub fn mssql_url_via_local_port(url: &str, port: u16) -> Result<String, DbError> {
    let mut parsed =
        url::Url::parse(url).map_err(|e| DbError::Connect(format!("invalid URL: {e}")))?;
    parsed
        .set_host(Some("127.0.0.1"))
        .map_err(|e| DbError::Connect(format!("rewriting URL host: {e}")))?;
    parsed
        .set_port(Some(port))
        .map_err(|_| DbError::Connect("rewriting URL port failed".into()))?;
    Ok(parsed.into())
}

/// Builds a password-free URL from the individual connection-form fields.
pub fn build_mssql_url(
    host: &str,
    port: &str,
    database: &str,
    user: &str,
    encrypt: &str,
) -> Result<String, DbError> {
    let port = if port.trim().is_empty() {
        "1433".to_string()
    } else {
        port.trim().to_string()
    };
    if host.trim().is_empty() {
        return Err(DbError::Connect("host must not be empty".into()));
    }
    let mut parsed = url::Url::parse("mssql://localhost").expect("static base URL parses");
    parsed
        .set_host(Some(host.trim()))
        .map_err(|e| DbError::Connect(format!("invalid host: {e}")))?;
    let port_num: u16 = port
        .parse()
        .map_err(|_| DbError::Connect(format!("invalid port: {port}")))?;
    if port_num == 0 {
        return Err(DbError::Connect("port must be between 1 and 65535".into()));
    }
    parsed
        .set_port(Some(port_num))
        .map_err(|_| DbError::Connect("invalid port".into()))?;
    parsed
        .set_username(user.trim())
        .map_err(|_| DbError::Connect("invalid user".into()))?;
    // Only set a path for a non-empty database, so an empty db field converges
    // with a pasted URL that has no path (both → no trailing `/`).
    let database = database.trim();
    if !database.is_empty() {
        parsed.set_path(&format!("/{database}"));
    }
    if !encrypt.is_empty() {
        parsed.set_query(Some(&format!("encrypt={encrypt}")));
    }
    // Route through the normalizer so a form host typed as `MyHost` and a
    // pasted `myhost` URL land on the same canonical locator.
    normalize_mssql_url(parsed.as_str())
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
    fn into_config(self) -> Config {
        let mut config = Config::new();
        config.host(&self.host);
        config.port(self.port);
        config.authentication(AuthMethod::sql_server(&self.user, &self.password));
        if let Some(database) = &self.database {
            config.database(database);
        }
        config.encryption(self.encryption);
        if self.trust_server_certificate {
            config.trust_cert();
        }
        config.application_name("dataview");
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

    /// Drops the client instead of returning it to the pool — for connections
    /// whose TDS stream may be left mid-response (driver-level errors, capped
    /// reads that abandoned a result set).
    fn discard(&mut self) {
        self.client = None;
    }
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
    fn new(config: Config) -> Self {
        MssqlPool {
            inner: Arc::new(PoolInner {
                config,
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
            None => connect_client(&self.inner.config).await?,
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
/// session password in via [`mssql_url_with_password`]. Validates the
/// connection with a `SELECT 1` round-trip.
pub async fn open_mssql(url: &str) -> Result<MssqlPool, DbError> {
    let config = parse_mssql_url(url)?.into_config();
    let pool = MssqlPool::new(config);
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

/// Opens a TCP connection and performs the TDS login, following at most one
/// Azure-style routing redirect, then applies [`SESSION_SETUP`].
async fn connect_client(config: &Config) -> Result<TdsClient, DbError> {
    let mut client = match connect_once(config.clone()).await {
        // Azure SQL can answer the login with "actually, talk to this other
        // node"; a single redirect is all the protocol calls for.
        Err(TdsError::Routing { host, port }) => {
            let mut redirected = config.clone();
            redirected.host(&host);
            redirected.port(port);
            connect_once(redirected).await
        }
        other => other,
    }
    .map_err(|e| DbError::Connect(friendly_connect_error(&e)))?;
    match run_batch(&mut client, SESSION_SETUP).await {
        Ok(()) => Ok(client),
        Err(e) => Err(DbError::Connect(friendly_connect_error(&e))),
    }
}

async fn connect_once(config: Config) -> Result<TdsClient, TdsError> {
    let io_err = |e: std::io::Error| TdsError::Io {
        kind: e.kind(),
        message: e.to_string(),
    };
    let tcp = TcpStream::connect(config.get_addr())
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

/// Runs a parameterized query and buffers the first result set.
async fn run_query(
    client: &mut TdsClient,
    sql: &str,
    params: &[Value],
) -> Result<QueryResult, TdsError> {
    let mut query = Query::new(sql.to_string());
    bind_params(&mut query, params);
    let mut stream = query.query(client).await?;
    let columns = match stream.columns().await? {
        Some(columns) => column_infos(columns),
        None => Vec::new(),
    };
    let mut rows = Vec::new();
    let mut result_sets = 0usize;
    while let Some(item) = stream.try_next().await? {
        match item {
            QueryItem::Metadata(_) => {
                result_sets += 1;
                // Only the first result set is kept — mirrors the sqlx
                // backends, where one statement yields one result.
                if result_sets > 1 {
                    break;
                }
            }
            QueryItem::Row(row) => rows.push(decode_row(&row)),
        }
    }
    Ok(QueryResult { columns, rows })
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
    settle(&mut conn, result, sql)
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
/// A truncated read abandons the rest of the server's response mid-stream;
/// tiberius would silently drain it before the *next* query on that
/// connection, which for a huge result would defer the whole download to some
/// innocent later query — so the connection is discarded instead (one
/// reconnect is far cheaper).
pub async fn query_capped(
    pool: &MssqlPool,
    sql: &str,
    params: &[Value],
    max_rows: u64,
    cell_cap: usize,
) -> Result<(QueryResult, bool), DbError> {
    let mut conn = pool.acquire().await?;
    let result = run_query_capped(conn.client(), sql, params, max_rows, cell_cap).await;
    let outcome = settle(&mut conn, result, sql)?;
    if outcome.1 {
        conn.discard();
    }
    Ok(outcome)
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
    let result = run_query_capped(tx.conn.client(), sql, &[], max_rows, cell_cap).await;
    settle(&mut tx.conn, result, sql)
}

/// Drains a query's row stream into a bounded [`QueryResult`], keeping at
/// most `max_rows` rows and capping each cell to `cell_cap` bytes; the bool
/// is whether rows existed past the cap.
async fn run_query_capped(
    client: &mut TdsClient,
    sql: &str,
    params: &[Value],
    max_rows: u64,
    cell_cap: usize,
) -> Result<(QueryResult, bool), TdsError> {
    let mut query = Query::new(sql.to_string());
    bind_params(&mut query, params);
    let mut stream = query.query(client).await?;
    let columns = match stream.columns().await? {
        Some(columns) => column_infos(columns),
        None => Vec::new(),
    };
    let mut rows: Vec<Vec<Value>> = Vec::new();
    let mut truncated = false;
    let mut result_sets = 0usize;
    while let Some(item) = stream.try_next().await? {
        match item {
            QueryItem::Metadata(_) => {
                result_sets += 1;
                if result_sets > 1 {
                    break;
                }
            }
            QueryItem::Row(row) => {
                // The cap+1'th row that reaches us proves there is more; stop
                // before decoding it so exactly `max_rows` rows are retained.
                if rows.len() as u64 >= max_rows {
                    truncated = true;
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
    Ok((QueryResult { columns, rows }, truncated))
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
                DbError::RowCountMismatch(format!(
                    "statement affected {affected} rows, expected {} — rolled back",
                    statement.expected_rows
                )),
            ));
        }
    }
    let commit = run_batch(conn.client(), "COMMIT TRAN").await;
    settle(&mut conn, commit, "COMMIT TRAN").map_err(|e| (None, e))
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
/// rows.
pub async fn execute_conn(tx: &mut MssqlTx, sql: &str) -> Result<u64, DbError> {
    let result = run_execute(tx.conn.client(), sql, &[]).await;
    settle(&mut tx.conn, result, sql)
}

/// Commits a script transaction — its statements all take effect. A failed
/// commit discards the connection (nothing was committed; the server rolls
/// the transaction back with the session).
pub async fn commit_tx(mut tx: MssqlTx) -> Result<(), DbError> {
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
/// the connection, which rolls the transaction back server-side anyway.
pub async fn rollback_tx(mut tx: MssqlTx) {
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

/// Minimal introspection STUB: tables and views with their columns from
/// INFORMATION_SCHEMA, enough for the schema tree and grid to render. No
/// primary keys, indexes, foreign keys, or identity/computed detection yet —
/// that full metadata parity is FRE-56.
pub async fn introspect(pool: &MssqlPool) -> Result<Vec<TableMeta>, DbError> {
    let map_err = |e: DbError| DbError::Introspect(e.message().to_string());

    let table_rows = query_with(
        pool,
        "SELECT TABLE_SCHEMA, TABLE_NAME, TABLE_TYPE \
         FROM INFORMATION_SCHEMA.TABLES \
         ORDER BY TABLE_SCHEMA, TABLE_NAME",
        &[],
    )
    .await
    .map_err(map_err)?;

    let column_rows = query_with(
        pool,
        "SELECT TABLE_SCHEMA, TABLE_NAME, COLUMN_NAME, DATA_TYPE, IS_NULLABLE, COLUMN_DEFAULT \
         FROM INFORMATION_SCHEMA.COLUMNS \
         ORDER BY TABLE_SCHEMA, TABLE_NAME, ORDINAL_POSITION",
        &[],
    )
    .await
    .map_err(map_err)?;

    let text = |row: &[Value], idx: usize| -> String {
        match row.get(idx) {
            Some(Value::Text(t)) => t.clone(),
            other => other.map(|v| v.display()).unwrap_or_default(),
        }
    };

    let mut tables: Vec<TableMeta> = Vec::with_capacity(table_rows.rows.len());
    for row in &table_rows.rows {
        tables.push(TableMeta {
            schema: Some(text(row, 0)),
            name: text(row, 1),
            kind: match text(row, 2).as_str() {
                "VIEW" => TableKind::View,
                _ => TableKind::Table,
            },
            columns: Vec::new(),
            indexes: Vec::new(),
            foreign_keys: Vec::new(),
        });
    }
    for row in &column_rows.rows {
        let schema = text(row, 0);
        let table = text(row, 1);
        let Some(idx) = tables
            .iter()
            .position(|t| t.schema.as_deref() == Some(schema.as_str()) && t.name == table)
        else {
            continue;
        };
        let default = match row.get(5) {
            Some(Value::Text(t)) => Some(t.clone()),
            _ => None,
        };
        tables[idx].columns.push(ColumnMeta {
            name: text(row, 2),
            type_name: text(row, 3),
            nullable: text(row, 4) == "YES",
            primary_key_position: None,
            default,
            generated: Generated::Never,
        });
    }
    Ok(tables)
}

fn column_infos(columns: &[tiberius::Column]) -> Vec<ColumnInfo> {
    columns
        .iter()
        .map(|c| ColumnInfo {
            name: c.name().to_string(),
        })
        .collect()
}

/// Decodes every cell of one fetched row into the backend-neutral [`Value`]
/// model.
fn decode_row(row: &Row) -> Vec<Value> {
    (0..row.len()).map(|idx| decode_value(row, idx)).collect()
}

/// Decodes scalar and rich SQL Server types into the backend-neutral
/// [`Value`] model. Rich types (dates, decimals, uuids, money, xml) render as
/// `Value::Text`, mirroring the Postgres backend's stringification style.
///
/// Cell data never errors the page: a type without a dedicated arm — or one
/// whose dedicated decode fails — degrades through [`decode_fallback`] to a
/// text read where possible, then to a `<typename>` marker.
fn decode_value(row: &Row, idx: usize) -> Value {
    let column_type = match row.columns().get(idx) {
        Some(column) => column.column_type(),
        None => ColumnType::Null,
    };
    decode_typed(row, idx, column_type).unwrap_or_else(|| decode_fallback(row, idx, column_type))
}

/// Shapes one `try_get` outcome: a decoded value, an SQL NULL, or `None` when
/// this arm cannot represent the cell (the caller then degrades).
fn opt<T>(result: tiberius::Result<Option<T>>, f: impl FnOnce(T) -> Value) -> Option<Value> {
    match result {
        Ok(Some(value)) => Some(f(value)),
        Ok(None) => Some(Value::Null),
        Err(_) => None,
    }
}

/// Type-specific decoding. Returns `None` both for types without a dedicated
/// arm and for values a dedicated arm cannot represent; the caller degrades
/// those via [`decode_fallback`].
fn decode_typed(row: &Row, idx: usize, column_type: ColumnType) -> Option<Value> {
    use chrono::{DateTime, FixedOffset, NaiveDate, NaiveDateTime, NaiveTime};
    match column_type {
        // bit renders as 0/1 — it IS numeric in T-SQL (no boolean literals).
        ColumnType::Bit | ColumnType::Bitn => {
            opt(row.try_get::<bool, _>(idx), |b| Value::Integer(b as i64))
        }
        ColumnType::Int1
        | ColumnType::Int2
        | ColumnType::Int4
        | ColumnType::Int8
        | ColumnType::Intn => decode_int(row, idx),
        ColumnType::Float4 | ColumnType::Float8 | ColumnType::Floatn => decode_float(row, idx),
        // money/smallmoney arrive from the driver as f64 (scaled by 1e-4);
        // render with the type's full 4-digit scale, matching how numeric
        // keeps its scale digits.
        ColumnType::Money | ColumnType::Money4 => opt(row.try_get::<f64, _>(idx), |m| {
            Value::Text(format!("{m:.4}"))
        }),
        // Exact decimal string from the driver's (i128 value, scale) pair —
        // must not round-trip through f64. tiberius's own Display is broken
        // for negative values, hence [`format_numeric`].
        ColumnType::Decimaln | ColumnType::Numericn => {
            opt(row.try_get::<tiberius::numeric::Numeric, _>(idx), |n| {
                Value::Text(format_numeric(&n))
            })
        }
        ColumnType::Guid => opt(row.try_get::<tiberius::Uuid, _>(idx), |u| {
            Value::Text(u.to_string())
        }),
        ColumnType::BigVarChar
        | ColumnType::BigChar
        | ColumnType::NVarchar
        | ColumnType::NChar
        | ColumnType::Text
        | ColumnType::NText => opt(row.try_get::<&str, _>(idx), |s| Value::Text(s.to_string())),
        ColumnType::BigVarBin | ColumnType::BigBinary | ColumnType::Image => {
            opt(row.try_get::<&[u8], _>(idx), |b| Value::Blob(b.to_vec()))
        }
        ColumnType::Xml => opt(row.try_get::<&tiberius::xml::XmlData, _>(idx), |x| {
            Value::Text(x.to_string())
        }),
        // Date/time family; `%.f` prints fractional seconds only when
        // non-zero, and trailing zeros are trimmed, matching the Postgres
        // backend's rendering. datetime2 carries up to 7 fractional digits
        // (100 ns), which chrono's nanosecond precision covers exactly.
        ColumnType::Datetime2 => opt(row.try_get::<NaiveDateTime, _>(idx), |ts| {
            Value::Text(trim_fraction(ts.format("%Y-%m-%d %H:%M:%S%.f").to_string()))
        }),
        // Legacy datetime/smalldatetime tick in 1/300 s, which the driver
        // converts to a repeating-decimal nanosecond value (".336666666");
        // round to the type's actual millisecond display precision (".337")
        // the way SQL Server itself prints it.
        ColumnType::Datetime | ColumnType::Datetime4 | ColumnType::Datetimen => {
            opt(row.try_get::<NaiveDateTime, _>(idx), |ts| {
                use chrono::SubsecRound as _;
                let ts = ts.round_subsecs(3);
                Value::Text(trim_fraction(ts.format("%Y-%m-%d %H:%M:%S%.f").to_string()))
            })
        }
        ColumnType::Daten => opt(row.try_get::<NaiveDate, _>(idx), |d| {
            Value::Text(d.format("%Y-%m-%d").to_string())
        }),
        ColumnType::Timen => opt(row.try_get::<NaiveTime, _>(idx), |t| {
            Value::Text(trim_fraction(t.format("%H:%M:%S%.f").to_string()))
        }),
        // datetimeoffset keeps its stored offset (it is real data, unlike
        // Postgres's timestamptz which the server sends as an instant).
        ColumnType::DatetimeOffsetn => opt(row.try_get::<DateTime<FixedOffset>, _>(idx), |dt| {
            let local = trim_fraction(dt.format("%Y-%m-%d %H:%M:%S%.f").to_string());
            Value::Text(format!("{local}{}", dt.format("%:z")))
        }),
        // sql_variant / UDT / unknown: no dedicated arm.
        _ => None,
    }
}

/// The integer tiers behind int/bigint/smallint/tinyint (and their nullable
/// `intn` wire form): the driver reports the *declared* width in the column
/// type but sends each cell at its actual width, so try widest-first.
fn decode_int(row: &Row, idx: usize) -> Option<Value> {
    if let Some(v) = opt(row.try_get::<i64, _>(idx), Value::Integer) {
        return Some(v);
    }
    if let Some(v) = opt(row.try_get::<i32, _>(idx), |i| Value::Integer(i as i64)) {
        return Some(v);
    }
    if let Some(v) = opt(row.try_get::<i16, _>(idx), |i| Value::Integer(i as i64)) {
        return Some(v);
    }
    opt(row.try_get::<u8, _>(idx), |i| Value::Integer(i as i64))
}

/// float(53)/float(24) and their nullable `floatn` wire form.
fn decode_float(row: &Row, idx: usize) -> Option<Value> {
    if let Some(v) = opt(row.try_get::<f64, _>(idx), Value::Real) {
        return Some(v);
    }
    opt(row.try_get::<f32, _>(idx), |f| Value::Real(f as f64))
}

/// Graceful degradation for values [`decode_typed`] can't produce: a text
/// read, then a `<typename>` marker. Infallible by design — one odd cell
/// must not take down the page.
fn decode_fallback(row: &Row, idx: usize, column_type: ColumnType) -> Value {
    if let Some(value) = opt(row.try_get::<&str, _>(idx), |s| Value::Text(s.to_string())) {
        return value;
    }
    Value::Text(format!("<{}>", format!("{column_type:?}").to_lowercase()))
}

/// Exact decimal string for a numeric/decimal value from its scaled i128 and
/// scale, keeping the full scale digits (`1.50` stays `1.50`), like the
/// Postgres backend's NUMERIC stringification. Hand-rolled because
/// `tiberius::numeric::Numeric`'s own Display mangles negative values
/// (it formats the integer and fraction parts independently, each with its
/// own minus sign).
fn format_numeric(n: &tiberius::numeric::Numeric) -> String {
    let scale = n.scale() as usize;
    let value = n.value();
    let sign = if value < 0 { "-" } else { "" };
    let digits = value.unsigned_abs().to_string();
    if scale == 0 {
        return format!("{sign}{digits}");
    }
    let padded = format!("{digits:0>width$}", width = scale + 1);
    let (int_part, frac_part) = padded.split_at(padded.len() - scale);
    format!("{sign}{int_part}.{frac_part}")
}

/// Trims trailing zeros from a chrono-formatted fractional second: `%.f`
/// pads to 3/6/9 digits ("09.500"), the display wants minimal ("09.5"). The
/// input must end with the seconds field; the fraction dot is the only dot.
fn trim_fraction(mut s: String) -> String {
    if s.contains('.') {
        while s.ends_with('0') {
            s.pop();
        }
        if s.ends_with('.') {
            s.pop();
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_with_password_splices_and_encodes() {
        let url =
            mssql_url_with_password("mssql://sa@db.example.com:1433/app", "p@ss w%rd").unwrap();
        assert_eq!(url, "mssql://sa:p%40ss%20w%25rd@db.example.com:1433/app");
    }

    #[test]
    fn normalize_strips_password_and_checks_scheme() {
        assert_eq!(
            normalize_mssql_url(" mssql://u:secret@h:1433/db?encrypt=off ").unwrap(),
            "mssql://u@h:1433/db?encrypt=off"
        );
        assert!(normalize_mssql_url("postgres://u@h/db").is_err());
        assert!(normalize_mssql_url("not a url").is_err());
    }

    #[test]
    fn normalize_canonicalizes_scheme_host_and_port() {
        // sqlserver → mssql, default port filled, host lowercased.
        assert_eq!(
            normalize_mssql_url("sqlserver://user@Db.Example.COM/app").unwrap(),
            "mssql://user@db.example.com:1433/app"
        );
        // Already canonical: idempotent.
        let canonical = "mssql://user@db.example.com:1433/app";
        assert_eq!(normalize_mssql_url(canonical).unwrap(), canonical);
    }

    #[test]
    fn normalize_rejects_a_zero_port() {
        assert!(normalize_mssql_url("mssql://user@host:0/db").is_err());
        assert!(normalize_mssql_url("mssql://user@host:1433/db").is_ok());
    }

    #[test]
    fn equivalent_urls_normalize_to_the_same_locator() {
        // The same server written different ways must collapse to one
        // locator, so a saved list dedups and the keyring key matches.
        let forms = [
            "mssql://user@host:1433/db",
            "sqlserver://user@host:1433/db",
            "mssql://user@host/db",
            "sqlserver://user@HOST/db",
            "mssql://user:pw@host/db",
        ];
        let canonical = normalize_mssql_url(forms[0]).unwrap();
        for form in forms {
            assert_eq!(normalize_mssql_url(form).unwrap(), canonical, "{form}");
        }
    }

    #[test]
    fn build_url_assembles_fields_and_defaults_port() {
        assert_eq!(
            build_mssql_url("db.example.com", "", "app", "sa", "on").unwrap(),
            "mssql://sa@db.example.com:1433/app?encrypt=on"
        );
        assert_eq!(
            build_mssql_url(" h ", "14330", "d", "u", "off").unwrap(),
            "mssql://u@h:14330/d?encrypt=off"
        );
        assert!(build_mssql_url("h", "not-a-port", "d", "u", "on").is_err());
    }

    #[test]
    fn build_url_rejects_a_zero_port_and_empty_host() {
        assert!(build_mssql_url("host", "0", "db", "user", "").is_err());
        assert!(build_mssql_url("host", "70000", "db", "user", "").is_err());
        assert!(build_mssql_url("  ", "1433", "db", "u", "").is_err());
    }

    #[test]
    fn form_and_paste_converge_for_an_empty_database() {
        let from_form = build_mssql_url("host", "", "", "user", "").unwrap();
        assert_eq!(from_form, "mssql://user@host:1433");
        assert_eq!(from_form, normalize_mssql_url("mssql://user@host").unwrap());
    }

    #[test]
    fn url_target_extracts_host_and_defaults_port() {
        assert_eq!(
            mssql_url_target("mssql://u@db.internal/app").unwrap(),
            ("db.internal".to_string(), 1433)
        );
        assert_eq!(
            mssql_url_target("mssql://u@[::1]:14330/app").unwrap(),
            ("::1".to_string(), 14330)
        );
        assert!(mssql_url_target("not a url").is_err());
    }

    #[test]
    fn url_via_local_port_rewrites_only_host_and_port() {
        assert_eq!(
            mssql_url_via_local_port("mssql://u@db.internal:1433/app?encrypt=off", 40123).unwrap(),
            "mssql://u@127.0.0.1:40123/app?encrypt=off"
        );
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

    #[test]
    fn format_numeric_keeps_scale_and_handles_negatives() {
        let n = |value: i128, scale: u8| tiberius::numeric::Numeric::new_with_scale(value, scale);
        assert_eq!(format_numeric(&n(12345, 2)), "123.45");
        assert_eq!(format_numeric(&n(-15, 1)), "-1.5");
        assert_eq!(format_numeric(&n(5, 3)), "0.005");
        assert_eq!(format_numeric(&n(-5, 3)), "-0.005");
        assert_eq!(format_numeric(&n(0, 2)), "0.00");
        assert_eq!(format_numeric(&n(1500, 2)), "15.00");
        assert_eq!(format_numeric(&n(42, 0)), "42");
        assert_eq!(format_numeric(&n(-42, 0)), "-42");
        // 38-digit values stay exact (i128 range, beyond f64 and
        // rust_decimal).
        assert_eq!(
            format_numeric(&n(
                99_999_999_999_999_999_999_999_999_999_999_999_999i128,
                4
            )),
            "9999999999999999999999999999999999.9999"
        );
    }

    #[test]
    fn trim_fraction_strips_padding_zeros() {
        assert_eq!(trim_fraction("12:34:56.500".into()), "12:34:56.5");
        assert_eq!(trim_fraction("12:34:56.000".into()), "12:34:56");
        assert_eq!(trim_fraction("12:34:56".into()), "12:34:56");
    }
}
