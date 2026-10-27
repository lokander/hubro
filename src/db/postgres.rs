//! Postgres backend: connecting, query execution (buffered, capped/streaming,
//! and export paths), script transactions, DDL retrieval, and full
//! multi-schema introspection — every user schema's tables and views with
//! columns, primary keys, indexes, and foreign keys, with extension
//! bookkeeping and child partitions marked internal.

use std::collections::hash_map::Entry;
use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::sync::Arc;
use std::time::Duration;

use sqlx::postgres::types::{Oid, PgInterval};
use sqlx::postgres::{
    PgConnectOptions, PgDatabaseError, PgErrorPosition, PgHasArrayType, PgPool, PgPoolOptions,
    PgRow, PgTypeKind, PgValueFormat, PgValueRef,
};
use sqlx::types::{JsonValue, Uuid};
use sqlx::{ConnectOptions as _, Connection as _, Row as _, TypeInfo as _, ValueRef as _};

use super::caps::{Restriction, INSTEAD_RULE};
use super::ddl::{
    create_table_sql, create_view_sql, terminate, ColumnExtra, Ddl, DdlObject, TableExtras,
};
use super::entra::EntraTokens;
use super::error::DbError;
use super::export::ExportFormat;
use super::pg_wire;
use super::rowkey::KeyTextPinned;
use super::schema::{
    push_index_row, table_index, ColumnMeta, ForeignKeyMeta, Generated, IndexConstraint, IndexMeta,
    Internal, TableKind, TableMeta, TypeDetail, TypeRef,
};
use super::script::final_read_may_close;
use super::sql::Dialect;
pub(super) use super::sqlx_common::Rest;
use super::sqlx_common::{self, get};
use super::staged::CheckedStatement;
use super::stats::{size_bytes, RowCount, TableStats};
use super::value::{real_from_f32, row_opt_text, row_text, Capped, QueryResult, RestOfRead, Value};

/// Splices a password into a Postgres URL — see [`super::url::with_password`].
pub fn url_with_password(url: &str, password: &str) -> Result<String, DbError> {
    super::url::with_password(url, password)
}

/// Canonicalizes a Postgres URL into the stable saved-connection locator —
/// see [`super::url::UrlScheme::normalize`] (`postgresql://` → `postgres://`,
/// default port 5432).
pub fn normalize_pg_url(url: &str) -> Result<String, DbError> {
    super::url::POSTGRES.normalize(url)
}

/// The host and port a Postgres URL points at (default port 5432) — see
/// [`super::url::UrlScheme::target`].
pub fn url_target(url: &str) -> Result<(String, u16), DbError> {
    super::url::POSTGRES.target(url)
}

/// Rewrites a URL to connect through a forwarded local port — see
/// [`super::url::via_local_port`].
pub fn url_via_local_port(url: &str, port: u16) -> Result<String, DbError> {
    super::url::via_local_port(url, port)
}

/// Builds a password-free URL from the individual connection-form fields —
/// see [`super::url::UrlScheme::build`] (TLS param `sslmode`).
pub fn build_url(
    host: &str,
    port: &str,
    database: &str,
    user: &str,
    sslmode: &str,
) -> Result<String, DbError> {
    super::url::POSTGRES.build(host, port, database, user, sslmode)
}

/// Which engine answered the Postgres wire handshake.
///
/// hubro speaks one protocol to a family of servers: stock PostgreSQL, the
/// extensions layered on it (TimescaleDB, Citus) which are the same engine and
/// share [`PgFlavor::Postgres`], and the reimplementations, which get their
/// own variant.
///
/// **Introspection never branches on this, and that is the intended state.** Both problems found while verifying CockroachDB looked like they
/// needed the engine's identity and turned out not to: a 64-bit
/// `ordinal_position` is fixed by pinning the width in SQL for everyone, and
/// its reserved catalog schemas are found through `table_type`, which is the
/// server's own classification. A catalog fact beats knowing the name every
/// time — it reports what this server actually does rather than what its name
/// implies, it needs no update when an engine changes behaviour between
/// versions, and it keeps intact the rule that internal objects are found by
/// catalog facts, never by name patterns. Reach for a flavor check only when
/// no catalog fact answers the question — which is why the backend's
/// capabilities and how it asks for EXPLAIN do read it (`registry.rs`).
///
/// It exists because one such question is already known: Materialize's
/// capabilities are a property of the engine, not of anything in its catalog,
/// and [`Capabilities`](super::caps::Capabilities) must be declared at
/// connect. It was added with the CockroachDB work rather than later because
/// the `version()` call that answers it is the liveness check the connect path
/// already made, so having the answer costs nothing — and because the finding
/// that most of these engines *don't* need it is worth recording in code
/// rather than losing.
///
/// Deliberately not a general version model: it answers "who is this" and
/// nothing else. Anything varying by version *within* one engine belongs in a
/// catalog query for the same reason as above.
///
/// Detected once at connect and never re-checked — the server on the other end
/// of an open connection does not change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PgFlavor {
    /// Stock PostgreSQL, and everything that is genuinely PostgreSQL
    /// underneath: TimescaleDB and Citus (extensions on a real server), and
    /// the hosted providers.
    Postgres,
    /// CockroachDB — a reimplementation of the wire protocol and SQL layer
    /// over its own storage.
    CockroachDB,
    /// YugabyteDB — the real PostgreSQL query layer over its own storage.
    Yugabyte,
    /// Materialize — a streaming engine speaking the Postgres wire protocol.
    Materialize,
    /// RisingWave — a streaming engine with real, editable tables, but no
    /// read-write transactions.
    RisingWave,
}

/// Identifies the engine from its `version()` string.
///
/// Every engine here names itself in that one string, and every one of them
/// *also* claims a PostgreSQL version for compatibility — Yugabyte reports
/// `PostgreSQL 15.12-YB-…`, Materialize reports `PostgreSQL 9.5 … (Materialize
/// 26.36.0)`. So the engine's own name is what identifies it, and stock
/// Postgres is the answer only when no reimplementation named itself. Getting
/// that backwards would file both of those as plain Postgres.
///
/// An unrecognized server is [`PgFlavor::Postgres`] because that is the
/// behaviour it will be treated with: the standard catalog path, no special
/// cases. A new engine misreads as Postgres and works as well as it did
/// before it was known, rather than failing on a name nobody has heard of.
fn detect_flavor(version: &str) -> PgFlavor {
    let version = version.to_ascii_lowercase();
    if version.contains("cockroachdb") {
        PgFlavor::CockroachDB
    } else if version.contains("materialize") {
        PgFlavor::Materialize
    } else if version.contains("risingwave") {
        PgFlavor::RisingWave
    } else if version.contains("-yb-") || version.contains("yugabyte") {
        PgFlavor::Yugabyte
    } else {
        PgFlavor::Postgres
    }
}

/// A Postgres-wire connection: the pool, plus which engine is on the other end.
///
/// The flavor travels with the pool rather than being re-derived where it is
/// needed, so no code path can act on a guess about the server — mirroring
/// [`MssqlPool`](super::sqlserver::MssqlPool), which likewise owns its own
/// connection state instead of exposing a bare driver handle.
#[derive(Clone)]
pub struct PgConn {
    pool: PgPool,
    flavor: PgFlavor,
    /// What opens each statement's transaction on a connection that may not
    /// write — see [`read_only_begin`].
    read_only_begin: &'static str,
    /// An Entra connection's login: where its token comes from, and the task
    /// that keeps the pool's password current — see [`open_postgres_entra`].
    entra: Option<EntraLogin>,
    /// Which settings key text depends on every session is given at login —
    /// all of them, unless a pooler refused the startup option
    /// ([`open_postgres_sized`]).
    pinned: KeyTextPinned,
}

/// An Entra connection's token provider, and the task renewing the pool's
/// password from it, aborted when the last clone of the connection goes.
#[derive(Clone)]
struct EntraLogin {
    tokens: Arc<EntraTokens>,
    _renewer: Arc<AbortOnDrop>,
}

struct AbortOnDrop(tokio::task::AbortHandle);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// How often an Entra connection's renewal task asks for the token. Well
/// inside [`RENEW_MARGIN`](super::entra::RENEW_MARGIN), so a token is
/// renewed before it expires even when a check is missed; and a check costs
/// nothing while the token is good. Read against the wall clock each time,
/// so a laptop waking from sleep past the expiry renews within one interval.
const RENEW_EVERY: Duration = Duration::from_secs(60);

/// Sets `pool`'s password to the token [`EntraTokens::current`] hands out,
/// renewing it first when it nears expiry. The token *is* the password on
/// Azure Postgres, and sqlx logs every connection it opens in with the
/// pool's options — a script run's, and the replacements for connections it
/// retires (idle for 10 minutes, or 30 minutes old) — so this is what keeps
/// those logins working past the first token's hour.
pub(super) async fn renew_login(pool: &PgPool, tokens: &EntraTokens) -> Result<(), DbError> {
    let token = tokens.current().await?;
    let options = (*pool.connect_options()).clone().password(token.expose());
    pool.set_connect_options(options);
    Ok(())
}

/// [`open_postgres`] for a connection that logs in with a Microsoft Entra
/// token: `url` carries the current token as its password, and a task renews
/// it into the pool's options every [`RENEW_EVERY`] ([`renew_login`]). A
/// script run asks too, before it takes its connection
/// ([`PgConn::renew_login`]), so it never waits a whole interval for a token
/// that has just expired, and an expired sign-in that cannot be renewed fails
/// it with a reason rather than the server's password error.
pub async fn open_postgres_entra(url: &str, tokens: Arc<EntraTokens>) -> Result<PgConn, DbError> {
    open_postgres_entra_every(url, tokens, RENEW_EVERY).await
}

/// [`open_postgres_entra`] with the renewal task's interval chosen by the
/// caller. Only a test passes anything but [`RENEW_EVERY`]: one that must see
/// the task renew without waiting a minute.
pub async fn open_postgres_entra_every(
    url: &str,
    tokens: Arc<EntraTokens>,
    every: Duration,
) -> Result<PgConn, DbError> {
    let mut conn = open_postgres(url).await?;
    let pool = conn.pool.clone();
    let task_tokens = Arc::clone(&tokens);
    let renewer = tokio::spawn(async move {
        loop {
            tokio::time::sleep(every).await;
            if pool.is_closed() {
                break;
            }
            // A refusal is the next login's to report; the task keeps asking,
            // and `EntraTokens` decides whether that reaches the network.
            let _ = renew_login(&pool, &task_tokens).await;
        }
    });
    conn.entra = Some(EntraLogin {
        tokens,
        _renewer: Arc::new(AbortOnDrop(renewer.abort_handle())),
    });
    Ok(conn)
}

impl PgConn {
    /// The underlying sqlx pool, for the execution paths that are identical on
    /// every flavor (which is nearly all of them).
    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    pub fn flavor(&self) -> PgFlavor {
        self.flavor
    }

    /// Which settings key text depends on this connection's sessions share —
    /// see [`KeyTextPinned`].
    pub fn pinned(&self) -> KeyTextPinned {
        self.pinned
    }

    /// The statement a marked connection's reads open each transaction with
    /// ([`ReadOnlyRun`](super::registry::ReadOnlyRun), and its browsing) — see
    /// [`read_only_begin`].
    pub fn read_only_begin(&self) -> &'static str {
        self.read_only_begin
    }

    /// For an Entra connection, makes the pool's password the current token
    /// ([`renew_login`]) — or fails with why it cannot be; nothing for any
    /// other. Asked before a script run takes its connection, which is always
    /// a new one.
    pub(super) async fn renew_login(&self) -> Result<(), DbError> {
        match &self.entra {
            Some(entra) => renew_login(&self.pool, &entra.tokens).await,
            None => Ok(()),
        }
    }
}

/// `BEGIN READ ONLY`: the engine-side half of a connection the user marked
/// read-only. Measured on all seven Postgres-wire engines, a write inside it
/// is refused by the server (`nextval()`, `setval()`, `INSERT`, `CREATE
/// TABLE`, …) while catalog reads, `EXPLAIN` and `SHOW` still run.
pub const BEGIN_READ_ONLY: &str = "BEGIN READ ONLY";

/// [`BEGIN_READ_ONLY`] for a server whose DDL leaves the transaction it was
/// issued in.
///
/// CockroachDB's `autocommit_before_ddl` (on by default) *commits* the open
/// transaction before a schema change and runs the change outside it, so a
/// `CREATE TABLE` inside `BEGIN READ ONLY` was measured to succeed, read-only
/// transaction or not. Turning the setting off for the one transaction makes
/// the same statement fail with "cannot execute CREATE TABLE in a read-only
/// transaction". `SET LOCAL` ends with the transaction it was issued in, so
/// the rest of the pool keeps the default — which matters, because turning it
/// off session-wide breaks `ALTER TABLE` on schema-locked tables (see
/// [`open_postgres`]).
pub const BEGIN_READ_ONLY_KEEPING_DDL: &str =
    "BEGIN READ ONLY; SET LOCAL autocommit_before_ddl = off";

/// Which statement opens a read-only transaction on a server, given whether
/// it has the `autocommit_before_ddl` setting.
///
/// Decided by the setting's presence in `pg_settings` rather than by flavor:
/// a CockroachDB older than the setting has nothing to turn off (its DDL stays
/// inside the transaction), and `SET LOCAL` of a setting the server does not
/// know would fail every read-only statement.
pub fn read_only_begin(has_autocommit_before_ddl: bool) -> &'static str {
    if has_autocommit_before_ddl {
        BEGIN_READ_ONLY_KEEPING_DDL
    } else {
        BEGIN_READ_ONLY
    }
}

/// Whether the server has CockroachDB's `autocommit_before_ddl` setting.
///
/// Asked only of CockroachDB — the one engine known to have it — so the other
/// flavors pay no extra round trip at connect; the flavor picks whether to ask,
/// and the catalog answers. A failed probe answers *yes*: the cost of a wrong
/// yes is a read-only statement that fails visibly, where a wrong no lets a
/// schema change escape the read-only transaction without a word.
async fn has_autocommit_before_ddl(pool: &PgPool, flavor: PgFlavor) -> bool {
    if flavor != PgFlavor::CockroachDB {
        return false;
    }
    sqlx::query_scalar::<_, i64>(
        "SELECT count(*) FROM pg_catalog.pg_settings WHERE name = 'autocommit_before_ddl'",
    )
    .fetch_one(pool)
    .await
    .map(|n| n > 0)
    .unwrap_or(true)
}

/// Connects to Postgres from a URL (`postgres://user@host:port/db?sslmode=…`).
/// The URL may carry a password; saved config never does — callers splice a
/// session password in via [`url_with_password`].
///
/// Note for anyone reaching for `after_connect` to set a per-engine session
/// default here: CockroachDB's `autocommit_before_ddl` is the obvious
/// candidate, since leaving it on costs a script its atomicity. Turning it off
/// was tried during CockroachDB verification and reverted — it makes every
/// `ALTER TABLE`/`CREATE INDEX` fail against a schema-locked table, which is
/// how CockroachDB creates tables by default, so it breaks a common operation
/// to fix a rarer one. `tests/db_cockroach.rs` pins the resulting behaviour.
pub async fn open_postgres(url: &str) -> Result<PgConn, DbError> {
    open_postgres_sized(url, 4).await
}

/// [`open_postgres`] with a pool of at most `max_connections`. Only a test
/// opens one smaller than the app's: session state such as a temp table is
/// visible on the one connection that made it, and even a strictly sequential
/// caller on a fresh pool gets two, because sqlx hands a finished connection
/// back to the pool in a spawned task, so the very next acquire finds the idle
/// queue empty and opens another.
pub async fn open_postgres_sized(url: &str, max_connections: u32) -> Result<PgConn, DbError> {
    let options = url
        .parse::<PgConnectOptions>()
        .map_err(|e| connect_error(&e))?;
    let pool_options = || PgPoolOptions::new().max_connections(max_connections);
    // The pins go in the startup packet's `options`, which a pooler may
    // refuse: then the pool connects without them, and the key types whose
    // text depends on them are refused instead ([`KeyTextPinned`]). Any other
    // failure is the connect's own and is reported as it is.
    let (pool, options_reach) = match pool_options()
        .connect_with(options.clone().options(key_text_pins(None)))
        .await
    {
        Ok(pool) => (pool, true),
        Err(e) if rejects_startup_options(&e) => (
            pool_options()
                .connect_with(options.clone())
                .await
                .map_err(|e| connect_error(&e))?,
            false,
        ),
        Err(e) => return Err(connect_error(&e)),
    };
    // The first connection's `lc_monetary` is the role's and database's
    // default as of now; every later connection is pinned to it. An engine
    // without the setting refuses the `SHOW` and has no `money` type either.
    let monetary: Option<String> = sqlx::query_scalar("SHOW lc_monetary")
        .fetch_one(&pool)
        .await
        .ok();
    // Whether each pin actually reaches the server is asked of it, not
    // assumed from a connect succeeding: a pooler can take one option and
    // refuse another (PgBouncer 1.21 and 1.26 take `IntervalStyle` and refuse
    // `lc_monetary`), or drop the ones it does not track without a word
    // (`ignore_startup_parameters = options` kept `IntervalStyle` and dropped
    // `lc_monetary`, measured on 1.26).
    let pinned = if options_reach {
        probe_pins(&options, monetary.as_deref()).await
    } else {
        KeyTextPinned::NONE
    };
    if pinned.lc_monetary {
        pool.set_connect_options(options.options(key_text_pins(monetary.as_deref())));
    }
    // `SELECT version()` in place of the `SELECT 1` this used to run: it is
    // the same liveness check — one round trip on a connection the pool has
    // already opened — and it answers who the server is at the same time, so
    // knowing the flavor costs nothing.
    let version: String = sqlx::query_scalar("SELECT version()")
        .fetch_one(&pool)
        .await
        .map_err(|e| connect_error(&e))?;
    let flavor = detect_flavor(&version);
    let read_only_begin = read_only_begin(has_autocommit_before_ddl(&pool, flavor).await);
    Ok(PgConn {
        pool,
        flavor,
        read_only_begin,
        entra: None,
        pinned,
    })
}

/// The session settings every pooled connection is given at login (as
/// `-c name=value` in the startup packet's `options`), so that the text one
/// session prints for a key is the text another reads back as the same value.
///
/// A key without a decoder is read through `::text`
/// ([`super::page::fetched_as_server_text`]) by whichever session served the
/// page, and parsed back by whichever serves the save. Session settings
/// differ between pooled connections not only by a script's `SET` (a script's
/// connection is closed after its run) but by `ALTER ROLE/DATABASE … SET`,
/// which reaches only the sessions opened after it — and the pool opens new
/// ones all the time. Measured on PostgreSQL 17, TimescaleDB, Citus and
/// YugabyteDB: after `ALTER ROLE … SET IntervalStyle = 'sql_standard'`, a new
/// session got it, and one given this option at login did not. A setting given
/// at login overrides the role's and the database's.
///
/// - **`DateStyle`, `TimeZone`, `extra_float_digits`** are not here because
///   sqlx already sends them as startup parameters on every connection
///   (`ISO, MDY`, `UTC`, `2`), which override role defaults the same way and
///   take precedence over `options` (measured: an `ALTER ROLE … SET DateStyle =
///   'SQL, DMY'` left new hubro sessions on `ISO, MDY`).
/// - **`IntervalStyle`** is pinned to `postgres`, the server default and the
///   style hubro spells interval keys in itself. All seven Postgres-wire
///   engines accepted it at login; Materialize supports no other value.
/// - **`lc_monetary`** is pinned to `monetary`, the value the pool's first
///   connection reported, rather than to a fixed locale: it decides how many
///   minor units a `money` value has, so pinning `C` would change what every
///   stored amount means to someone whose database says otherwise. `None` —
///   Materialize and RisingWave, which lack the setting and the `money` type —
///   pins nothing, and so does CockroachDB in effect: it has only `C.UTF-8`,
///   no `money` type, and fails the probe below.
///
/// Because a setting given at login beats a role's default, a role-level
/// `IntervalStyle` (say `iso_8601`) does not apply in hubro's own sessions:
/// there, intervals print in the `postgres` style.
///
/// A pooler may refuse these options, or drop them: whether each reaches the
/// server is asked of the server ([`probe_pins`]), and a key type whose
/// setting did not is refused rather than trusted
/// ([`super::rowkey::KeyTextPinned`]).
///
/// `search_path` is deliberately absent: it decides name resolution for
/// everything the user runs, so it is not hubro's to pin, and the key types
/// whose text depends on it are refused instead
/// ([`super::rowkey::key_column_addressable`]).
///
/// Values are escaped for the `options` string, which splits on whitespace
/// and takes a backslash as an escape.
pub(crate) fn key_text_pins(monetary: Option<&str>) -> Vec<(&'static str, String)> {
    let escape = |value: &str| value.replace('\\', "\\\\").replace(' ', "\\ ");
    let mut pins = vec![("IntervalStyle", "postgres".to_string())];
    if let Some(monetary) = monetary {
        pins.push(("lc_monetary", escape(monetary)));
    }
    pins
}

/// Which pins reach the server: one probe session is given *other* values
/// than the pool's ([`pin_probe`]), so a pin a pooler dropped silently shows
/// as the server's own default rather than passing for one that held. When
/// the probe cannot connect with both, each is tried alone; a probe that
/// fails for any reason counts its pin as absent, which only refuses keys.
async fn probe_pins(options: &PgConnectOptions, monetary: Option<&str>) -> KeyTextPinned {
    async fn reported(
        options: PgConnectOptions,
        settings: &[(&str, String)],
    ) -> Option<Vec<String>> {
        let mut conn = options
            .options(settings.iter().map(|(name, value)| (*name, value.as_str())))
            .connect()
            .await
            .ok()?;
        let mut values = Vec::new();
        for (name, _) in settings {
            values.push(
                sqlx::query_scalar::<_, String>(&format!("SHOW {name}"))
                    .fetch_one(&mut conn)
                    .await
                    .ok()?,
            );
        }
        let _ = conn.close().await;
        Some(values)
    }
    let probe = pin_probe(monetary);
    let held = |settings: &[(&str, String)], values: &[String]| {
        settings
            .iter()
            .zip(values)
            .all(|((_, wanted), got)| wanted.eq_ignore_ascii_case(got))
    };
    let mut pinned = KeyTextPinned::NONE;
    if let Some(values) = reported(options.clone(), &probe).await {
        pinned.interval_style = held(&probe[..1], &values[..1]);
        pinned.lc_monetary = probe.len() > 1 && held(&probe[1..], &values[1..]);
        return pinned;
    }
    for (at, setting) in probe.iter().enumerate() {
        let one = std::slice::from_ref(setting);
        let ok = reported(options.clone(), one)
            .await
            .is_some_and(|values| held(one, &values));
        match at {
            0 => pinned.interval_style = ok,
            _ => pinned.lc_monetary = ok,
        }
    }
    pinned
}

/// The probe session's settings for [`probe_pins`]: each differs from the
/// value the pool pins and from the server's usual default, so the server
/// reporting it back proves the option reached it. `IntervalStyle` first;
/// `lc_monetary` only when the server has the setting (`monetary` is the
/// first session's value), as `C` — or `POSIX` where that value already is
/// `C`, both of which every PostgreSQL accepts.
fn pin_probe(monetary: Option<&str>) -> Vec<(&'static str, String)> {
    let mut probe = vec![("IntervalStyle", "iso_8601".to_string())];
    if let Some(monetary) = monetary {
        let other = if monetary.eq_ignore_ascii_case("C") {
            "POSIX"
        } else {
            "C"
        };
        probe.push(("lc_monetary", other.to_string()));
    }
    probe
}

/// Whether a failed connect is a pooler refusing the startup packet's
/// `options` — the one refusal [`open_postgres_sized`] answers by connecting
/// again without the pins.
fn rejects_startup_options(err: &sqlx::Error) -> bool {
    err.as_database_error()
        .is_some_and(|db| startup_options_refusal(db.code().as_deref(), db.message()))
}

/// [`rejects_startup_options`] over plain data, so a test can execute it.
///
/// Matched on the code *and* the wording, because the code alone says
/// nothing: `08P01` is a protocol violation, which PgBouncer also sends for a
/// failed SASL login (measured). The two wordings are PgBouncer's, measured
/// in front of PostgreSQL 17: 1.18 refuses `options` itself (`unsupported
/// startup parameter: options`); 1.21 and 1.26 parse it and refuse a
/// setting they do not track (`unsupported startup parameter in options:
/// lc_monetary`). Anything else — a wrong password, an unreachable host, a
/// pooler refusing some other parameter — is not retried: connecting again
/// without the pins could not change it.
fn startup_options_refusal(code: Option<&str>, message: &str) -> bool {
    code == Some("08P01")
        && (message == "unsupported startup parameter: options"
            || message.starts_with("unsupported startup parameter in options: "))
}

/// SQLSTATE `invalid_password`. PostgreSQL's `auth_failed` (auth.c) raises it
/// for a failed `password`, `md5` or `scram-sha-256` login only; every other
/// refusal there is `28000`, which a `pg_hba.conf` reject was measured to be.
/// Under password auth it also covers an unknown role, since the server will
/// not say which roles exist. Measured: a wrong password, an unknown role and
/// a missing password are all `28P01`, and a wrong password naming a database
/// that does not exist is `28P01` rather than `3D000` — authentication is
/// checked first.
const INVALID_PASSWORD: &str = "28P01";

/// Turns a failed connect into the error the connect flow acts on. See
/// [`connect_error_from`].
fn connect_error(err: &sqlx::Error) -> DbError {
    let sqlstate = err.as_database_error().and_then(|db| db.code());
    connect_error_from(sqlstate.as_deref(), friendly_connect_error(err))
}

/// The classification itself, over plain data so a test can execute it.
///
/// Decided on the SQLSTATE and nothing else, because the connect flow deletes
/// the stored password on [`DbError::CredentialsRejected`]: a SQLSTATE is the
/// part of the error PostgreSQL defines as stable, where the message is
/// translatable (`lc_messages`) and free to change wording between releases.
/// An unknown database (`3D000`) or a
/// missing `CONNECT` privilege (`42501`) is a [`DbError::Connect`] — the
/// password worked, and asking for another cannot help.
///
/// `28P01` is the server's wrong-credentials code, not proof: a role whose
/// `VALID UNTIL` has passed gets it too (measured), and so, presumably, does a
/// pooler refusing on the server's behalf. CockroachDB v26.2 in secure mode
/// sends `28P01` for a wrong password, an unknown user and a missing password
/// alike (measured), so it prompts as Postgres does. Another Postgres-wire
/// engine that refused a password with a different SQLSTATE would get no
/// prompt at all — the connect would report the error, and a password could
/// be entered only by editing the connection.
fn connect_error_from(sqlstate: Option<&str>, message: String) -> DbError {
    if sqlstate == Some(INVALID_PASSWORD) {
        DbError::CredentialsRejected(message)
    } else {
        DbError::Connect(message)
    }
}

/// Categorizes common failure modes so the connections screen reads well:
/// auth, network/DNS, TLS, wrong-server.
///
/// Words only. Whether the password is to blame — the question that costs a
/// stored secret — is [`connect_error_from`]'s, on the SQLSTATE; nothing may
/// decide it from this text.
fn friendly_connect_error(err: &sqlx::Error) -> String {
    let msg = err.to_string();
    let lower = msg.to_lowercase();
    if lower.contains("password authentication failed") {
        format!("authentication failed — {msg}")
    } else if lower.contains("role") && lower.contains("does not exist") {
        format!("unknown role — {msg}")
    } else if lower.contains("unexpected response from sslrequest") {
        // Something answered the Postgres handshake with garbage — usually a
        // different database server (e.g. SQL Server) on that host/port.
        format!("the server doesn't appear to be Postgres — check the host and port — {msg}")
    } else if lower.contains("unsupportedcertversion") {
        // rustls refuses to parse an X.509 v1 certificate, so the connection
        // dies before any `sslmode` policy gets a say — including `prefer`,
        // which reads as "encrypt if you can" but cannot fall back once the
        // TLS handshake has begun. libpq connects to such servers happily, so
        // without this the failure looks like hubro being broken rather than
        // the certificate being two decades out of date.
        //
        // Not exotic: the official `citusdata/citus` image generates exactly
        // such a certificate on first boot. Verified that a *valid* v3
        // self-signed certificate connects fine at both `prefer` and
        // `require`, so this is specifically about the version, not about
        // hubro verifying more strictly than libpq does.
        format!(
            "the server's TLS certificate is X.509 v1, which modern TLS libraries reject — \
             reissue it as v3, or use sslmode=disable if the network is trusted — {msg}"
        )
    } else if lower.contains("tls") || lower.contains("ssl") {
        format!("TLS error — {msg}")
    } else if lower.contains("connection refused")
        || lower.contains("timed out")
        || lower.contains("failed to lookup")
    {
        format!("network error — {msg}")
    } else {
        msg
    }
}

type PgQuery<'q> = sqlx::query::Query<'q, sqlx::Postgres, sqlx::postgres::PgArguments>;

/// The Postgres half of the generic sqlx paths ([`sqlx_common::SqlxBackend`]).
/// sqlx-postgres sends a statement whole, so the text needs no door.
pub(super) struct PgBackend;

impl sqlx_common::SqlxBackend for PgBackend {
    type DB = sqlx::Postgres;
    type Text<'q> = &'q str;

    fn prepare(sql: &str) -> Result<&str, DbError> {
        Ok(sql)
    }

    fn sql<'q>(text: Self::Text<'q>) -> &'q str {
        text
    }

    fn query<'q>(text: Self::Text<'q>) -> PgQuery<'q> {
        sqlx::query(text)
    }

    /// NULL caveat: `Value::Null` binds as `None::<String>`, i.e. a NULL of
    /// type text. That is fine for the current uses (filter values are text),
    /// but Postgres rejects a text NULL for non-text columns in e.g.
    /// `SET col = $n`. Cell editing solves this at the SQL level: the staged
    /// SQL builders (`staged::ParamSql::value_sql`) render NULL values inline
    /// as the literal `NULL` instead of binding them, so a NULL never reaches
    /// this function from an edit.
    ///
    /// Statement-cache caveat: each variant binds as its own Postgres type, and
    /// sqlx caches a prepared statement by its SQL text alone, with the types of
    /// its first execution. A builder whose one SQL text can be executed with
    /// different variants in one position must make them one type first — the
    /// staged builders bind everything under a cast as text for exactly this
    /// reason (see `staged::ParamSql::value_sql`).
    fn bind<'q>(query: PgQuery<'q>, value: &Value) -> PgQuery<'q> {
        match value {
            Value::Null => query.bind(None::<String>),
            Value::Integer(i) => query.bind(*i),
            Value::Real(r) => query.bind(*r),
            Value::Text(t) => query.bind(t.clone()),
            Value::Blob(b) => query.bind(b.clone()),
        }
    }

    fn decode(row: &PgRow, idx: usize) -> Result<Value, DbError> {
        decode_value(row, idx)
    }

    fn map_err(err: sqlx::Error, sql: &str) -> DbError {
        query_error(err, sql)
    }
}

/// Runs an arbitrary query, decoding every cell into the backend-neutral
/// [`Value`] model. Free-form SQL whose result is shown to the user, so a
/// zero-row result recovers its headers.
pub async fn query(pool: &PgPool, sql: &str) -> Result<QueryResult, DbError> {
    sqlx_common::query_all::<PgBackend>(pool, sql).await
}

/// Like [`query`], with bound parameters — see [`sqlx_common::query_with`],
/// including why it recovers no headers.
pub async fn query_with(
    pool: &PgPool,
    sql: &str,
    params: &[Value],
) -> Result<QueryResult, DbError> {
    sqlx_common::query_with::<PgBackend, _>(pool, sql, params).await
}

/// [`query_with`] for a connection the user marked read-only: the statement
/// runs in a read-only transaction of its own ([`begin_read_only`]), always
/// rolled back, so the engine refuses whatever the object being browsed does
/// as a write — a view over `nextval()`, or over a function that inserts.
///
/// Returns the read's outcome and the rollback's separately, for the caller
/// to combine the way a marked script's statements are
/// (`registry::read_only_outcome`). The connection goes back to the pool: a
/// browse runs only SQL hubro built, so nothing a script could leave on a
/// session is left by one, and a reconnect per page would cost far more than
/// the transaction does. What a browsed object's functions leave on the
/// session outside the transaction (a session-level advisory lock) stays,
/// as it does on any connection — the class
/// [`WriteProtection::ReadOnly`](super::caps::WriteProtection::ReadOnly)
/// names.
///
/// **A connection whose `BEGIN` failed is closed, not returned.** CockroachDB's
/// `BEGIN` is two statements ([`BEGIN_READ_ONLY_KEEPING_DDL`]); if the first
/// opens the transaction and the second fails, sqlx reports the failure with
/// no transaction it knows to roll back, and the pool would take the
/// connection back inside an aborted transaction — every later read on it
/// failing with "current transaction is aborted" (pinned by
/// `a_browse_whose_begin_fails_does_not_strand_its_connection`).
pub async fn read_only_query_with(
    pg: &PgConn,
    sql: &str,
    params: &[Value],
) -> (Result<QueryResult, DbError>, Result<(), DbError>) {
    read_only_query_on(pg.pool(), pg.read_only_begin(), sql, params).await
}

/// [`read_only_query_with`] with the `BEGIN` given, so a test can make it fail.
async fn read_only_query_on(
    pool: &PgPool,
    begin: &'static str,
    sql: &str,
    params: &[Value],
) -> (Result<QueryResult, DbError>, Result<(), DbError>) {
    let mut conn = match pool.acquire().await {
        Ok(conn) => conn,
        Err(e) => return (Err(query_error(e, sql)), Ok(())),
    };
    // The work happens inside the arm, so the transaction's borrow of `conn`
    // has ended by the time a failed `BEGIN` closes it.
    let error = match begin_read_only(&mut conn, begin).await {
        Ok(mut tx) => {
            let result = sqlx_common::query_with::<PgBackend, _>(&mut *tx, sql, params).await;
            return (result, end_read_only(tx).await);
        }
        Err(error) => error,
    };
    conn.close_on_drop();
    (Err(error), Ok(()))
}

/// A free-form query with its rows and cells capped — see
/// [`sqlx_common::query_capped`].
pub async fn query_capped(
    pool: &PgPool,
    sql: &str,
    params: &[Value],
    max_rows: u64,
    cell_cap: usize,
) -> Result<(QueryResult, bool), DbError> {
    sqlx_common::query_capped::<PgBackend>(pool, sql, params, max_rows, cell_cap).await
}

/// Runs a non-row statement on a single connection (e.g. one borrowed from a
/// transaction) rather than the pool, returning affected rows — the write
/// path for statements inside an atomically-wrapped script.
pub async fn execute_conn(
    conn: &mut sqlx::postgres::PgConnection,
    sql: &str,
) -> Result<u64, DbError> {
    sqlx_common::execute(conn, sql, |e| query_error(e, sql)).await
}

/// Ends a script's hold on its connection
/// ([`ScriptConn`](super::registry::ScriptConn)): rolls back any transaction
/// the script left open, and reports whether there was one.
///
/// Whether one is open is the server's word, not hubro's reading of the
/// script: every `ReadyForQuery` message carries the session's transaction
/// status, on every Postgres-wire engine, and sqlx keeps the latest. Its only
/// public window onto it is the check `begin_with` makes after sending its
/// statement — a custom statement that leaves the session outside a
/// transaction fails with [`sqlx::Error::BeginFailed`]. So the probe is
/// `SELECT 1` sent as a "begin": it succeeds exactly when the session was
/// already inside a transaction, and the handle it returns then rolls that
/// transaction back.
///
/// A probe that fails outright is refused by the session's state, or the
/// connection is broken; a `ROLLBACK` follows either way, ends whatever block
/// the session is in, and fails only on a broken connection — then an error is
/// returned: the connection's state is unknown, and hubro cannot say what
/// closing it discarded. Whether the `ROLLBACK` undid anything is decided by
/// [`refused_probe_rolled_back`]: yes, except for the one refusal known to
/// follow a commit — CockroachDB's `SHOW COMMIT TIMESTAMP` leaves the session
/// in a block whose transaction is already *committed* (status `CommitWait`,
/// refusing statements with `25000`), and reporting that as a rollback claimed
/// the script's committed rows were gone.
pub async fn end_script_conn(
    conn: &mut sqlx::postgres::PgConnection,
    flavor: PgFlavor,
) -> Result<bool, DbError> {
    let rolled_back = match sqlx::Connection::begin_with(&mut *conn, "SELECT 1").await {
        Ok(open) => {
            return open
                .rollback()
                .await
                .map(|()| true)
                .map_err(|e| DbError::Query(e.to_string()))
        }
        Err(sqlx::Error::BeginFailed) => return Ok(false),
        Err(refused) => refused_probe_rolled_back(
            flavor,
            refused
                .as_database_error()
                .and_then(|e| e.code())
                .as_deref(),
        ),
    };
    execute_conn(conn, "ROLLBACK").await.map(|_| rolled_back)
}

/// Whether the `ROLLBACK` sent after the end-of-run probe was refused with
/// `sqlstate` undid something.
///
/// Yes for every refusal but one, because the `ROLLBACK` discards whatever an
/// open transaction holds, and a clean report over discarded work is the
/// silent loss this path exists to stop. An aborted transaction refuses with
/// `25P02`; Materialize's write-only transaction (opened by an `INSERT`)
/// refuses a table read with `25000` — measured, though it takes the probe's
/// `SELECT 1`, and after refusing the read it is aborted anyway.
///
/// The one exception is CockroachDB's commit-wait block: `SHOW COMMIT
/// TIMESTAMP` commits the transaction and leaves the session refusing every
/// statement with `25000` until the block ends, so ending it undoes nothing.
/// Decided by the flavor as well as the code, since the code alone also means
/// Materialize's refusal of uncommitted work.
fn refused_probe_rolled_back(flavor: PgFlavor, sqlstate: Option<&str>) -> bool {
    !(flavor == PgFlavor::CockroachDB && sqlstate == Some("25000"))
}

/// A script's last statement, when it returns rows, and the end of the
/// script's hold on its connection — see
/// [`ScriptConn::finish_with_read`](super::registry::ScriptConn::finish_with_read).
///
/// Unless [`final_read_may_close`] says the statement can neither touch the
/// transaction nor write, the read is an ordinary one
/// ([`writable_capped_read`]) followed by the ordinary end. Otherwise
/// whether a transaction is open is asked *before* the read:
///
/// - A transaction is open: a read the row cap cut short is followed by
///   nothing — the caller closes the connection, which ends the transaction
///   server-side and is reported as the rollback it is.
/// - Nothing is open, on an engine that says whether a statement has written
///   ([`knows_whether_written`]): the rest is cancelled if the statement has
///   written nothing, and otherwise read to its end here, so that an error
///   past the cap — a unique violation, say — is the read's, reported as
///   any other statement's is.
/// - Nothing is open on any other engine: a cut-short read is left
///   unfinished, and its rest read in the background before the connection
///   closes ([`AfterRead::DrainClose`](super::registry::AfterRead::DrainClose)) — closing at once would abort a write
///   the gate cannot see (`SELECT f()` for a function that writes), and
///   waiting for it would cost every capped read the rest of its result.
pub async fn finish_script_conn_with_read(
    conn: &mut sqlx::postgres::PgConnection,
    pg: &PgConn,
    known: &mut Option<Session>,
    sql: &str,
    max_rows: u64,
    cell_cap: usize,
) -> super::registry::FinishedRead {
    use super::registry::{AfterRead, FinishedRead};
    let flavor = pg.flavor();
    if !final_read_may_close(sql, Dialect::Postgres) {
        let read = writable_capped_read(conn, pg, known, sql, max_rows, cell_cap).await;
        let ended = end_script_conn(conn, flavor).await;
        return FinishedRead {
            read,
            ended,
            after: AfterRead::Close,
        };
    }
    if knows_whether_written(flavor) {
        if let Some(Probed {
            session,
            in_block,
            timeout_off,
            ..
        }) = probe_session(conn, known).await
        {
            let target = CancelTarget::Backend(session);
            let stopper = (!in_block).then_some(Stopper {
                pool: pg.pool(),
                target: &target,
                sql,
                unwritten_only: true,
                timeout_off: Some(timeout_off),
            });
            let otherwise = if in_block { Rest::Leave } else { Rest::Read };
            let read = capped_read(conn, sql, max_rows, cell_cap, stopper, otherwise).await;
            if in_block && matches!(&read, Ok(capped) if capped.truncated) {
                // Stopped by the close that follows.
                return FinishedRead {
                    read: read.map(|capped| Capped {
                        rest: RestOfRead::Stopped,
                        ..capped
                    }),
                    ended: Ok(true),
                    after: AfterRead::Close,
                };
            }
            let ended = end_script_conn(conn, flavor).await;
            return FinishedRead {
                read,
                ended,
                after: AfterRead::Close,
            };
        }
    }
    let refused = match sqlx::Connection::begin_with(&mut *conn, "SELECT 1").await {
        Ok(mut open) => {
            let read = capped_read(&mut open, sql, max_rows, cell_cap, None, Rest::Leave).await;
            if let Ok(capped) = &read {
                if capped.truncated {
                    // Stopped by the close that follows.
                    return FinishedRead {
                        read: read.map(|capped| Capped {
                            rest: RestOfRead::Stopped,
                            ..capped
                        }),
                        ended: Ok(true),
                        after: AfterRead::Close,
                    };
                }
            }
            let ended = open
                .rollback()
                .await
                .map(|()| true)
                .map_err(|e| DbError::Query(e.to_string()));
            return FinishedRead {
                read,
                ended,
                after: AfterRead::Close,
            };
        }
        Err(sqlx::Error::BeginFailed) => false,
        // An aborted transaction (which refuses the read too, with the
        // server's own message) or a broken connection: the ordinary end
        // sorts them out.
        Err(_) => true,
    };
    if refused {
        let read = capped_read(conn, sql, max_rows, cell_cap, None, Rest::Read).await;
        let ended = end_script_conn(conn, flavor).await;
        return FinishedRead {
            read,
            ended,
            after: AfterRead::Close,
        };
    }
    let read = capped_read(conn, sql, max_rows, cell_cap, None, Rest::Leave).await;
    if matches!(&read, Ok(capped) if capped.truncated) {
        return FinishedRead {
            read,
            ended: Ok(false),
            after: AfterRead::DrainClose,
        };
    }
    let ended = end_script_conn(conn, flavor).await;
    FinishedRead {
        read,
        ended,
        after: AfterRead::Close,
    }
}

/// Reads the rest of a cut-short read in a background task, then closes the
/// connection — the end of a script run whose last read the row cap cut short
/// with nothing open, on an engine that cannot say whether the statement has
/// written (see [`finish_script_conn_with_read`]).
///
/// Read rather than abandoned, because closing a connection mid-result
/// aborts the statement, and a read can write (`SELECT f()`); in the
/// background, because the run must not wait for it (measured, 1.3 s for 3M
/// rows on localhost). Closed rather than returned, as every script run's
/// connection is ([`ScriptConn`](super::registry::ScriptConn)). The ping is
/// what the pool sent on a connection's return: sqlx answers it only once
/// every pending response has been read. The connection keeps its pool slot
/// until it is closed, as it did while the pool read it. A failure in the
/// rest ends the statement's own transaction with it, after the run has
/// reported — which is why such a read reports its rest
/// [`RestOfRead::Unconfirmed`] (`registry::rest_after`) rather than claiming
/// it ran, and the outcome follows on the returned [`PendingRest`](super::value::PendingRest).
///
/// Unlike every other rest, this one is read by sqlx in one poll — the stream
/// is gone — so it holds one of the runtime's worker threads until it is
/// done. Not the thread the run is polled on: a `tokio::spawn`ed task runs on
/// the worker pool, and the SQL tab's run on the UI thread.
///
/// What the rest met is sent on that [`PendingRest`](super::value::PendingRest) once it has been
/// read ([`late_rest`]), so whoever shows the run can say so; the run does not
/// wait for it.
pub(super) fn drain_then_close(
    mut conn: sqlx::pool::PoolConnection<sqlx::Postgres>,
) -> super::value::PendingRest {
    let (sender, receiver) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        let ping = sqlx::Connection::ping(&mut *conn).await;
        let _ = conn.close().await;
        // Nobody may be listening any more (a plain `run_script`); that is
        // theirs to decide.
        let _ = sender.send(late_rest(ping));
    });
    super::value::PendingRest(receiver)
}

/// How the rest of a read ended, from the ping that read it to its end
/// ([`drain_then_close`]). The server's own error ended the statement, and
/// undid it with its writes. Any other error — the connection lost, a
/// protocol failure — leaves unknown whether the statement had finished by
/// then. Over plain data so a test can execute it.
pub(super) fn late_rest(ping: Result<(), sqlx::Error>) -> super::value::LateRest {
    use super::value::LateRest;
    match ping {
        Ok(()) => LateRest::Finished,
        Err(sqlx::Error::Database(error)) => LateRest::Failed(error.to_string()),
        Err(_) => LateRest::Unknown,
    }
}

/// Opens the read-only transaction one statement of a read-only script runs
/// in — see [`read_only_begin`] for the statement, and
/// [`ReadOnlyRun`](super::registry::ReadOnlyRun) for why it is one per
/// statement.
pub async fn begin_read_only<'c>(
    conn: &'c mut sqlx::postgres::PgConnection,
    begin: &'static str,
) -> Result<sqlx::Transaction<'c, sqlx::Postgres>, DbError> {
    sqlx::Connection::begin_with(conn, begin)
        .await
        .map_err(|e| DbError::Query(e.to_string()))
}

/// Ends a read-only statement's transaction. Always a rollback: nothing in it
/// may stay, and on Materialize — whose read-only transaction accepts a
/// `CREATE TABLE` and was measured to make it permanent on `COMMIT` — the
/// rollback is what keeps it out.
///
/// Unlike [`sqlx_common::rollback_tx`] this reports a failure, so the caller
/// stops using a connection whose transaction state is no longer known.
pub async fn end_read_only(tx: sqlx::Transaction<'_, sqlx::Postgres>) -> Result<(), DbError> {
    tx.rollback()
        .await
        .map_err(|e| DbError::Query(e.to_string()))
}

/// One server session, named so that a statement on it can be cancelled
/// from another connection: its backend's pid, and when that backend
/// started. A pid alone is not a name — through anything that can route the
/// cancelling connection to another server (a load balancer in front of
/// several), the same pid can be an unrelated session there — and a pid
/// with its start time, to the microsecond, is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Session {
    pid: i32,
    started_us: i64,
}

/// How a statement the row cap cut short is cancelled from another session.
pub(super) enum CancelTarget {
    /// `pg_cancel_backend` on this session's backend.
    Backend(Session),
    /// CockroachDB's `CANCEL QUERIES` on whatever this session is running.
    /// It has no `pg_cancel_backend` (measured: "unknown function").
    CockroachSession(String),
}

/// How each engine is asked to cancel a statement from another session —
/// `None` where it cannot be. Measured with a read-only transaction streaming
/// `generate_series(1, 200000000)`, cancelled after its first thousand rows:
///
/// - PostgreSQL and YugabyteDB: `pg_cancel_backend` ends it, with `57014`.
/// - CockroachDB: `CANCEL QUERIES` does, with `57014` again.
/// - Materialize: `pg_cancel_backend` answers true and changes nothing — the
///   result is computed in full before its first row is sent, and the rest
///   went on arriving for minutes.
/// - RisingWave: `pg_backend_pid()` is `0`, and cancelling it answers false.
///
/// Decided by the flavor, as no catalog says whether a cancel takes effect.
pub(super) fn cancel_kind(flavor: PgFlavor) -> Option<CancelKind> {
    match flavor {
        PgFlavor::Postgres | PgFlavor::Yugabyte => Some(CancelKind::Backend),
        PgFlavor::CockroachDB => Some(CancelKind::CockroachSession),
        PgFlavor::Materialize | PgFlavor::RisingWave => None,
    }
}

/// Which [`CancelTarget`] an engine is cancelled through ([`cancel_kind`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum CancelKind {
    Backend,
    CockroachSession,
}

/// Whether the engine says, of a running statement, if its transaction has
/// written anything yet — which is what lets a *writable* connection's read
/// be cancelled at the row cap ([`Stopper::unwritten_only`]). Only where the
/// answer was measured to be the server's own and correct:
///
/// - PostgreSQL: `pg_stat_activity.backend_xid` is empty for a plain read and
///   set from the first write on — by a function a `SELECT` calls, too — for
///   the rest of the transaction. Citus (a `Postgres` flavor) the same, for a
///   function writing to a distributed table on a single-node cluster.
/// - YugabyteDB: has the column, and left it empty while a function the
///   `SELECT` called was inserting rows. Trusting it would cancel writes.
/// - CockroachDB: `pg_stat_activity` has no row for the session at all.
/// - Materialize and RisingWave cannot cancel a statement anyway
///   ([`cancel_kind`]).
///
/// No transaction id is **not** the same as no effect, and the cancel
/// undoes the whole statement, rows before the cap included — measured on
/// PostgreSQL 17, through both the atomic and the sequential path: a
/// `set_config(…, false)` the statement made was gone afterwards, and a
/// listener received none of the `pg_notify`s it sent. Neither takes a
/// transaction id; nor do `dblink_exec`, `COPY … TO PROGRAM` or `lo_export`
/// run through a function, whose effects are outside the transaction. The
/// user's decision is to cancel anyway, and the editor says what that means
/// (`truncation_notice`).
///
/// Where it is `false`, a writable connection's cut-short read is read to
/// its end, as every one was before.
pub(super) fn knows_whether_written(flavor: PgFlavor) -> bool {
    match flavor {
        PgFlavor::Postgres => true,
        PgFlavor::Yugabyte
        | PgFlavor::CockroachDB
        | PgFlavor::Materialize
        | PgFlavor::RisingWave => false,
    }
}

/// What a writable read needs to know before it runs, in one round trip:
/// the session, whether it is inside a transaction block, whether that
/// transaction has written already, and whether `statement_timeout` is off.
///
/// **Inside a block** is `transaction_timestamp() <> statement_timestamp()`,
/// sent as a simple query, where a statement outside a block is a
/// transaction of its own and starts when it does: measured, the two were
/// equal 200 times out of 200 outside a block, and never inside one. (Over
/// the extended protocol they differ outside a block too, so this needs
/// `raw_sql`.) It rests on the timestamps' microsecond resolution: inside a
/// block they could be equal only if the probe ran within the microsecond
/// the transaction began, a round trip earlier. That wrong "outside" would
/// leave a cancelled read without its savepoint, and the script's next
/// statements would fail with "current transaction is aborted" — loudly, and
/// with nothing written lost, since the transaction had written nothing. A
/// wrong "inside" cannot happen (outside a block the two are one instant);
/// it would only make the savepoint fail and the read be read to its end.
const SESSION_PROBE: &str = "SELECT pg_backend_pid(), \
    (SELECT (extract(epoch FROM backend_start) * 1000000)::int8 \
     FROM pg_stat_activity WHERE pid = pg_backend_pid()), \
    transaction_timestamp() = statement_timestamp(), \
    txid_current_if_assigned() IS NOT NULL, \
    current_setting('statement_timeout') = '0'";

/// [`SESSION_PROBE`] without the start time, which only `pg_stat_activity`
/// has: reading that view made the probe cost 0.5 ms a read on localhost,
/// against a tenth of that without it. The start time of a backend the run
/// has seen before is known already ([`probe_session`]).
const BLOCK_PROBE: &str = "SELECT pg_backend_pid(), \
    transaction_timestamp() = statement_timestamp(), \
    txid_current_if_assigned() IS NOT NULL, \
    current_setting('statement_timeout') = '0'";

/// [`SESSION_PROBE`]'s answer — [`BLOCK_PROBE`]'s when the backend is
/// `known`'s, the full probe (and `known` updated) when it is not: the first
/// read of a run, or a pooler that moved the session to another backend.
/// `None` when it cannot be had — the read is then read to its end.
pub(super) async fn probe_session(
    conn: &mut sqlx::postgres::PgConnection,
    known: &mut Option<Session>,
) -> Option<Probed> {
    if let Some(session) = *known {
        let row = sqlx::raw_sql(BLOCK_PROBE)
            .fetch_one(&mut *conn)
            .await
            .ok()?;
        let pid: i32 = row.try_get(0).ok()?;
        if pid == session.pid {
            return Some(Probed {
                session,
                in_block: !row.try_get::<bool, _>(1).ok()?,
                written: row.try_get(2).ok()?,
                timeout_off: row.try_get(3).ok()?,
            });
        }
    }
    let row = sqlx::raw_sql(SESSION_PROBE)
        .fetch_one(&mut *conn)
        .await
        .ok()?;
    let session = session_from(&row, 0)?;
    *known = Some(session);
    Some(Probed {
        session,
        in_block: !row.try_get::<bool, _>(2).ok()?,
        written: row.try_get(3).ok()?,
        timeout_off: row.try_get(4).ok()?,
    })
}

/// [`probe_session`]'s answer.
#[derive(Debug, Clone, Copy)]
pub(super) struct Probed {
    session: Session,
    in_block: bool,
    /// The transaction has written to a table already. Only ever true inside
    /// a block: a statement outside one is a transaction of its own, which
    /// has not started yet.
    written: bool,
    /// `statement_timeout` is off, so a cancellation the read ends in cannot
    /// be the timeout's ([`sqlx_common::cancel_landed`]).
    timeout_off: bool,
}

/// The [`Session`] in a row's columns `at` and `at + 1`.
fn session_from(row: &sqlx::postgres::PgRow, at: usize) -> Option<Session> {
    let pid: i32 = row.try_get(at).ok()?;
    let started_us: Option<i64> = row.try_get(at + 1).ok()?;
    Some(Session {
        pid,
        started_us: started_us?,
    })
}

/// How long sending a cancel may take before the rest of the result is read
/// instead: it needs another of the pool's connections, and every one may be
/// busy.
const CANCEL_WAIT: Duration = Duration::from_secs(5);

/// What may stop a read the row cap cut short: a cancel for `target`, sent
/// from another of `pool`'s connections.
pub(super) struct Stopper<'a> {
    pub(super) pool: &'a PgPool,
    pub(super) target: &'a CancelTarget,
    /// The statement being read. The cancel also requires the session to be
    /// running it, so a session that has moved on — or, through a
    /// transaction-pooling PgBouncer, a server backend now serving another
    /// client — is left alone.
    pub(super) sql: &'a str,
    /// Cancel only a statement whose transaction has written nothing — what
    /// a writable connection's read needs ([`knows_whether_written`]). Asked
    /// of the server in the same statement that sends the cancel
    /// ([`send_cancel`]). A read-only connection's needs no such question:
    /// the engine refuses its writes, and its transaction is rolled back
    /// regardless.
    pub(super) unwritten_only: bool,
    /// Whether the session's `statement_timeout` was off before the read
    /// ([`sqlx_common::cancel_landed`]); `None` if not asked.
    pub(super) timeout_off: Option<bool>,
}

impl Stopper<'_> {
    /// At the cap: [`Rest::Cancelled`] if the server took the cancel,
    /// `otherwise` if it did not, could not be asked in time, or the
    /// statement has written.
    async fn at_cap(self, otherwise: Rest) -> Rest {
        let sent = tokio::time::timeout(
            CANCEL_WAIT,
            send_cancel(self.pool, self.target, self.sql, self.unwritten_only),
        )
        .await;
        if matches!(sent, Ok(Ok(true))) {
            Rest::Cancelled {
                timeout_off: self.timeout_off,
            }
        } else {
            otherwise
        }
    }
}

/// Cancels `pid` if it is still the session that started at `$2` and still
/// running `$3` — `pg_stat_activity.query` is the statement's text, cut at
/// `track_activity_query_size`, so a prefix of it.
const CANCEL_SESSION: &str = "SELECT pg_cancel_backend(pid) FROM pg_stat_activity \
     WHERE pid = $1 AND (extract(epoch FROM backend_start) * 1000000)::int8 = $2 \
     AND state = 'active' AND query <> '' AND starts_with($3, query)";

/// [`CANCEL_SESSION`], and only while the session's transaction has no id —
/// has written nothing to a table.
const CANCEL_SESSION_UNWRITTEN: &str = "SELECT pg_cancel_backend(pid) FROM pg_stat_activity \
     WHERE pid = $1 AND (extract(epoch FROM backend_start) * 1000000)::int8 = $2 \
     AND state = 'active' AND query <> '' AND starts_with($3, query) \
     AND backend_xid IS NULL";

/// Asks the server to cancel what `target` is running; true if it took the
/// request. A statement already finished is not cancelled — the session is
/// idle, or running something else.
///
/// With `unwritten_only`, the transaction's having written nothing is read
/// in the very statement that sends the cancel, so the answer is not one
/// round trip stale. A write that lands between that read and the cancel can
/// only be made by a row past the cap — the rows before it have all been
/// produced — and a write past the cap is what cancelling at the cap gives
/// up.
async fn send_cancel(
    pool: &PgPool,
    target: &CancelTarget,
    sql: &str,
    unwritten_only: bool,
) -> Result<bool, sqlx::Error> {
    match target {
        CancelTarget::Backend(session) => sqlx::query_scalar::<_, bool>(if unwritten_only {
            CANCEL_SESSION_UNWRITTEN
        } else {
            CANCEL_SESSION
        })
        .bind(session.pid)
        .bind(session.started_us)
        .bind(sql)
        .fetch_optional(pool)
        .await
        .map(|cancelled| cancelled == Some(true)),
        // No way to ask CockroachDB whether the session has written
        // (`knows_whether_written`), so never on that condition.
        CancelTarget::CockroachSession(_) if unwritten_only => Ok(false),
        CancelTarget::CockroachSession(session) => sqlx::query(
            "CANCEL QUERIES IF EXISTS \
             (SELECT query_id FROM [SHOW CLUSTER STATEMENTS] WHERE session_id = $1)",
        )
        .bind(session)
        .execute(pool)
        .await
        .map(|done| done.rows_affected() > 0),
    }
}

/// The session a marked connection's run holds, asked once when the run
/// takes its connection, on the engines cancelled by backend
/// ([`cancel_kind`]). Each statement runs in a read-only transaction of its
/// own; the cancel's match on the statement's text keeps a pooler's switch
/// of backends between them from cancelling anyone else's.
pub(super) async fn read_only_session(
    conn: &mut sqlx::postgres::PgConnection,
    flavor: PgFlavor,
) -> Option<Probed> {
    if cancel_kind(flavor) != Some(CancelKind::Backend) {
        return None;
    }
    probe_session(conn, &mut None).await
}

/// Who a marked connection's read is cancelled through — the run's
/// [`Session`], or on CockroachDB the session id, asked inside the
/// statement's transaction (`CANCEL QUERIES` has no text to match) — and
/// whether `statement_timeout` was off, where asked. A `SET` in a marked run
/// is rolled back with its statement, so the run's own answer holds.
pub(super) async fn read_only_target(
    conn: &mut sqlx::postgres::PgConnection,
    flavor: PgFlavor,
    session: Option<Probed>,
) -> Result<Option<(CancelTarget, Option<bool>)>, DbError> {
    match cancel_kind(flavor) {
        None => Ok(None),
        Some(CancelKind::Backend) => Ok(session.map(|probed| {
            (
                CancelTarget::Backend(probed.session),
                Some(probed.timeout_off),
            )
        })),
        Some(CancelKind::CockroachSession) => sqlx::query_scalar::<_, String>("SHOW session_id")
            .fetch_one(conn)
            .await
            .map(|id| Some((CancelTarget::CockroachSession(id), None)))
            .map_err(|e| DbError::Query(e.to_string())),
    }
}

/// Runs `sql` on `conn`, bounded by the row cap and `cell_cap`, with the
/// rest of a result the row cap cuts short cancelled by `stopper` where it
/// can be, and otherwise handled as `otherwise` says. The rest is read here,
/// giving the thread turns ([`sqlx_common::collect_capped_then`]) — never by
/// the connection's next question, which reads it in one poll that nothing
/// can interrupt.
pub(super) async fn capped_read(
    conn: &mut sqlx::postgres::PgConnection,
    sql: &str,
    max_rows: u64,
    cell_cap: usize,
    stopper: Option<Stopper<'_>>,
    otherwise: Rest,
) -> Result<Capped, DbError> {
    let unwritten_only = stopper
        .as_ref()
        .is_some_and(|stopper| stopper.unwritten_only);
    let stream = sqlx::query(sql).fetch(&mut *conn);
    let mut capped = sqlx_common::collect_capped_then(
        stream,
        max_rows,
        cell_cap,
        decode_value,
        |e| query_error(e, sql),
        || async move {
            match stopper {
                Some(stopper) => stopper.at_cap(otherwise).await,
                None => otherwise,
            }
        },
    )
    .await?;
    if unwritten_only && capped.rest.stopped() {
        capped.rest = RestOfRead::CancelledUnwritten;
    }
    sqlx_common::fill_headers(&mut capped.result, &mut *conn, sql).await;
    Ok(capped)
}

async fn run_raw(
    conn: &mut sqlx::postgres::PgConnection,
    sql: &'static str,
) -> Result<(), DbError> {
    sqlx::raw_sql(sql)
        .execute(&mut *conn)
        .await
        .map(|_| ())
        .map_err(|e| DbError::Query(e.to_string()))
}

/// The savepoint a writable read runs in inside a transaction. A cancelled
/// statement aborts the transaction it runs in, and measured, every statement
/// after it then failed with "current transaction is aborted"; rolling back
/// to this instead leaves the transaction as it was before the read.
///
/// One thing changes for a read that *fails* inside it: the error aborts the
/// savepoint, and the transaction is left aborted still holding the locks it
/// took before it, until it is rolled back — where without one, the error
/// released them at once (measured: a deadlock victim's earlier row lock kept
/// the other transaction waiting until the victim rolled back). Every run
/// rolls back straight after a failed statement, so the difference lasts as
/// long as that rollback takes.
const READ_SAVEPOINT: &str = "SAVEPOINT hubro_capped_read";

/// A sequential script's read on a writable connection: when the row cap cuts
/// it short and its transaction has **written nothing to a table**
/// ([`knows_whether_written`]), the rest is cancelled; otherwise it is read to
/// its end here. Writes that rows past the cap would have made are not made,
/// and the cancel undoes what the statement did before the cap, too: that is
/// the trade, decided for the user.
///
/// One round trip before the read ([`probe_session`]) names the session and
/// says whether the script has a transaction open, and whether it has
/// written — `known` is the run's session, kept from read to read. Outside one the
/// statement is its own transaction, and the cancel ends it alone. Inside
/// one, the read runs in [`READ_SAVEPOINT`], released right after it — the
/// script may roll back to a savepoint of its own, which would take an
/// unreleased one of hubro's with it — for two more, unless the transaction
/// has written already and the read cannot be cancelled anyway.
pub(super) async fn writable_capped_read(
    conn: &mut sqlx::postgres::PgConnection,
    pg: &PgConn,
    known: &mut Option<Session>,
    sql: &str,
    max_rows: u64,
    cell_cap: usize,
) -> Result<Capped, DbError> {
    if !knows_whether_written(pg.flavor()) {
        return capped_read(conn, sql, max_rows, cell_cap, None, Rest::Read).await;
    }
    let Some(Probed {
        session,
        in_block,
        written,
        timeout_off,
    }) = probe_session(conn, known).await
    else {
        return capped_read(conn, sql, max_rows, cell_cap, None, Rest::Read).await;
    };
    if written {
        // Never cancelled, so no savepoint either.
        return capped_read(conn, sql, max_rows, cell_cap, None, Rest::Read).await;
    }
    let target = CancelTarget::Backend(session);
    let stopper = Stopper {
        pool: pg.pool(),
        target: &target,
        sql,
        unwritten_only: true,
        timeout_off: Some(timeout_off),
    };
    if !in_block {
        return capped_read(conn, sql, max_rows, cell_cap, Some(stopper), Rest::Read).await;
    }
    if run_raw(conn, READ_SAVEPOINT).await.is_err() {
        // An aborted transaction refuses the read too, with the server's own
        // message.
        return capped_read(conn, sql, max_rows, cell_cap, None, Rest::Read).await;
    }
    let read = capped_read(conn, sql, max_rows, cell_cap, Some(stopper), Rest::Read).await;
    if let Ok(capped) = &read {
        if capped.rest.stopped() {
            run_raw(conn, "ROLLBACK TO SAVEPOINT hubro_capped_read").await?;
        }
        run_raw(conn, "RELEASE SAVEPOINT hubro_capped_read").await?;
    }
    read
}

/// What hubro's own script transaction has learned for its reads
/// ([`transaction_capped_read`]): the session, asked with the first read and
/// kept, since the transaction holds one backend for the whole run; whether
/// the transaction has written, which once true stays true; and whether
/// [`READ_SAVEPOINT`] is still open.
#[derive(Debug, Default)]
pub struct TransactionReads {
    session: Option<Option<Session>>,
    written: bool,
    savepoint_open: bool,
}

/// [`READ_SAVEPOINT`] with the session asked in the same round trip — the
/// first read of a script transaction — and what every read asks: whether
/// the transaction has written, and whether `statement_timeout` is off.
const FIRST_READ_SAVEPOINT: &str = "SAVEPOINT hubro_capped_read; \
    SELECT pg_backend_pid(), \
    (SELECT (extract(epoch FROM backend_start) * 1000000)::int8 \
     FROM pg_stat_activity WHERE pid = pg_backend_pid()), \
    txid_current_if_assigned() IS NOT NULL, \
    current_setting('statement_timeout') = '0'";

/// [`READ_SAVEPOINT`] after the previous read's, with the same questions.
const NEXT_READ_SAVEPOINT: &str = "RELEASE SAVEPOINT hubro_capped_read; \
    SAVEPOINT hubro_capped_read; \
    SELECT txid_current_if_assigned() IS NOT NULL, \
    current_setting('statement_timeout') = '0'";

/// [`READ_SAVEPOINT`] alone, with the same questions.
const READ_SAVEPOINT_ASKING: &str = "SAVEPOINT hubro_capped_read; \
    SELECT txid_current_if_assigned() IS NOT NULL, \
    current_setting('statement_timeout') = '0'";

/// [`writable_capped_read`] in hubro's own script transaction (the atomic
/// path), for one round trip per read: the savepoint is opened together with
/// the release of the previous read's, and released before a statement hubro
/// runs as a write ([`release_read_savepoint`]) rather than straight after the
/// read — so those statements never run inside one, where each would take a
/// subtransaction id, and a script of many reads and writes would overflow
/// the 64 the server tracks per session. A write made *by* a read
/// (`SELECT f()`, a data-modifying `WITH`) does run inside the read's
/// savepoint. The script's statements cannot touch savepoints themselves: a
/// script that does is not run in hubro's transaction.
///
/// Once the transaction is known to have written, no read in it can be
/// cancelled, so the reads after that skip the savepoint and its round trip.
pub(super) async fn transaction_capped_read(
    conn: &mut sqlx::postgres::PgConnection,
    pg: &PgConn,
    reads: &mut TransactionReads,
    sql: &str,
    max_rows: u64,
    cell_cap: usize,
) -> Result<Capped, DbError> {
    if !knows_whether_written(pg.flavor()) || reads.written || reads.session == Some(None) {
        return capped_read(conn, sql, max_rows, cell_cap, None, Rest::Read).await;
    }
    let (opener, asks_session) = match reads.session {
        None => (FIRST_READ_SAVEPOINT, true),
        Some(_) if reads.savepoint_open => (NEXT_READ_SAVEPOINT, false),
        Some(_) => (READ_SAVEPOINT_ASKING, false),
    };
    let answer = sqlx::raw_sql(opener).fetch_one(&mut *conn).await.ok();
    reads.savepoint_open = answer.is_some();
    let at = if asks_session { 2 } else { 0 };
    let known = reads.session.flatten();
    let asked = answer.and_then(|row| {
        let session = if asks_session {
            session_from(&row, 0)
        } else {
            known
        };
        let written: bool = row.try_get(at).ok()?;
        let timeout_off: bool = row.try_get(at + 1).ok()?;
        Some((session, written, timeout_off))
    });
    if asks_session {
        reads.session = Some(asked.and_then(|(session, ..)| session));
    }
    let Some((Some(session), written, timeout_off)) = asked else {
        return capped_read(conn, sql, max_rows, cell_cap, None, Rest::Read).await;
    };
    if written {
        reads.written = true;
        return capped_read(conn, sql, max_rows, cell_cap, None, Rest::Read).await;
    }
    let target = CancelTarget::Backend(session);
    let stopper = Stopper {
        pool: pg.pool(),
        target: &target,
        sql,
        unwritten_only: true,
        timeout_off: Some(timeout_off),
    };
    let read = capped_read(conn, sql, max_rows, cell_cap, Some(stopper), Rest::Read).await;
    if matches!(&read, Ok(capped) if capped.rest.stopped()) {
        run_raw(conn, "ROLLBACK TO SAVEPOINT hubro_capped_read").await?;
    }
    read
}

/// Releases the savepoint the last read of hubro's script transaction left
/// open ([`transaction_capped_read`]) — before a write, so the write runs in
/// the transaction itself.
pub(super) async fn release_read_savepoint(
    conn: &mut sqlx::postgres::PgConnection,
    reads: &mut TransactionReads,
) -> Result<(), DbError> {
    if !reads.savepoint_open {
        return Ok(());
    }
    reads.savepoint_open = false;
    run_raw(conn, "RELEASE SAVEPOINT hubro_capped_read").await
}

/// [`execute_conn`] with bound parameters — see
/// [`sqlx_common::execute_with`].
pub async fn execute_with_conn(
    conn: &mut sqlx::postgres::PgConnection,
    sql: &str,
    params: &[Value],
) -> Result<u64, DbError> {
    sqlx_common::execute_with::<PgBackend, _>(conn, sql, params).await
}

/// Streams a query to `out` in the given format — see
/// [`sqlx_common::export_query`].
pub async fn export(
    pool: &PgPool,
    sql: &str,
    params: &[Value],
    format: ExportFormat,
    out: &mut impl Write,
) -> Result<u64, DbError> {
    let mut conn = pool.acquire().await.map_err(|e| query_error(e, sql))?;
    sqlx_common::export_query::<PgBackend>(&mut conn, sql, params, format, out).await
}

/// [`export`] for a connection the user marked read-only, in a read-only
/// transaction that is always rolled back — see [`read_only_query_with`],
/// whose shape and reasons this shares — a connection whose `BEGIN` failed
/// is closed here too. The describe that recovers an empty export's header
/// runs inside the same transaction.
pub async fn read_only_export(
    pg: &PgConn,
    sql: &str,
    params: &[Value],
    format: ExportFormat,
    out: &mut impl Write,
) -> (Result<u64, DbError>, Result<(), DbError>) {
    read_only_export_on(pg.pool(), pg.read_only_begin(), sql, params, format, out).await
}

/// [`read_only_export`] with the `BEGIN` given, so a test can make it fail.
async fn read_only_export_on(
    pool: &PgPool,
    begin: &'static str,
    sql: &str,
    params: &[Value],
    format: ExportFormat,
    out: &mut impl Write,
) -> (Result<u64, DbError>, Result<(), DbError>) {
    let mut conn = match pool.acquire().await {
        Ok(conn) => conn,
        Err(e) => return (Err(query_error(e, sql)), Ok(())),
    };
    let error = match begin_read_only(&mut conn, begin).await {
        Ok(mut tx) => {
            let result =
                sqlx_common::export_query::<PgBackend>(&mut tx, sql, params, format, out).await;
            return (result, end_read_only(tx).await);
        }
        Err(error) => error,
    };
    conn.close_on_drop();
    (Err(error), Ok(()))
}

/// Executes a statement without decoding rows, returning the driver's
/// affected-row count.
pub async fn execute(pool: &PgPool, sql: &str) -> Result<u64, DbError> {
    sqlx_common::execute(pool, sql, |e| query_error(e, sql)).await
}

/// Executes parameterized writes inside ONE transaction, committing only
/// when every statement affected exactly its `expected_rows` rows — see
/// [`sqlx_common::execute_all_checked`].
pub async fn execute_all_checked(
    pool: &PgPool,
    statements: &[CheckedStatement],
) -> Result<(), (Option<usize>, DbError)> {
    sqlx_common::execute_all_checked::<PgBackend>(pool, statements).await
}

/// Builds a query error, appending "line L, column C" when the server
/// reported an error cursor. Postgres sends the position as a 1-based
/// *character* index into the query text; positions into internally
/// generated queries ([`PgErrorPosition::Internal`]) don't map to the user's
/// text and are ignored.
fn query_error(err: sqlx::Error, sql: &str) -> DbError {
    let mut message = err.to_string();
    if let sqlx::Error::Database(db_err) = &err {
        if let Some(pg) = db_err.try_downcast_ref::<PgDatabaseError>() {
            if let Some(PgErrorPosition::Original(position)) = pg.position() {
                if let Some((line, column)) = line_col(sql, position) {
                    message.push_str(&format!(" (line {line}, column {column})"));
                }
            }
        }
    }
    if is_transient(&err) {
        return DbError::Transient(message);
    }
    DbError::Query(message)
}

/// SQLSTATE `40001` — `serialization_failure`, the standard code for "this
/// failed against a concurrent change; run it again".
///
/// The code, never the message. Two of the engines in this family raise it for
/// reasons whose *text* shares nothing: YugabyteDB's catalog snapshot going
/// stale mid-query reports `MISMATCHED_SCHEMA` with a pair of internal version
/// numbers, and CockroachDB's transaction retries report a conflict. Both
/// arrive as `40001`, which is what makes one rule enough — and what keeps
/// hubro from matching on an engine-internal string that its authors never
/// promised to keep.
///
/// Deliberately just the one code. `40P01` (deadlock) is retryable in the same
/// sense but means genuine contention rather than a racing schema change.
///
/// On stock PostgreSQL this is near-inert without needing a flavor check: a
/// catalog read runs in its own implicit transaction, which at the default READ
/// COMMITTED cannot raise `40001`. Near-, not entirely — a server with
/// `default_transaction_isolation = serializable` applies it to implicit
/// transactions too, and a hot standby reports recovery conflicts as `40001`.
/// Both are cases where running the read once more is the right answer anyway,
/// so the classification needs no PostgreSQL-specific carve-out either way.
fn is_transient(err: &sqlx::Error) -> bool {
    let sqlx::Error::Database(db_err) = err else {
        return false;
    };
    db_err.code().as_deref() == Some("40001")
}

/// The error a failed catalog read reports: [`DbError::Transient`] when the
/// server called it retryable ([`is_transient`]), so the caller's one retry
/// (`registry::retry_transient`) can act on it, and an introspection failure
/// like any other otherwise. One mapping for every catalog read, so none can
/// lose the retry by spelling it differently.
fn catalog_error(err: sqlx::Error) -> DbError {
    if is_transient(&err) {
        DbError::Transient(err.to_string())
    } else {
        DbError::Introspect(err.to_string())
    }
}

/// Maps a 1-based character position into 1-based line and column numbers.
/// Returns `None` for positions outside the text (except one-past-the-end,
/// which the server reports e.g. for input that stops too early).
fn line_col(sql: &str, position: usize) -> Option<(usize, usize)> {
    if position == 0 {
        return None;
    }
    let mut line = 1usize;
    let mut column = 1usize;
    let mut seen = 0usize;
    for c in sql.chars() {
        seen += 1;
        if seen == position {
            return Some((line, column));
        }
        if c == '\n' {
            line += 1;
            column = 1;
        } else {
            column += 1;
        }
    }
    (position == seen + 1).then_some((line, column))
}

/// Why a streaming engine's source is not editable.
///
/// Worded for the object rather than the engine: both Materialize and
/// RisingWave report `SOURCE`, and naming one of them in a message the
/// other also shows would be worse than saying nothing.
const STREAMING_SOURCE: &str =
    "A source is written by the engine from an external system, not by hand.";

/// Why a streaming engine's sink is neither editable nor browsable.
///
/// A sink is the only object here with no rows *at all* — it writes outward, to
/// Kafka or another database. Without this it fell through to an ordinary
/// table, which offered editing on nothing and failed on open with the
/// engine's own "table or source not found".
///
/// Carried by [`Restriction::NoRows`] rather than `Declared`, so the sentence
/// below is what the *grid* shows in place of that failure, not only what the
/// editing affordance shows.
const STREAMING_SINK: &str =
    "A sink writes to an external system; it has no rows of its own to show.";

/// Which optional catalog columns a server actually has.
///
/// Every Postgres-wire engine claims an `information_schema`, and they do not
/// agree on what is in it: Materialize's `columns` view has eleven columns to
/// stock Postgres's forty-four, and `pg_index.indnkeyatts` only exists where
/// there are INCLUDE columns to exclude. Selecting a column the server does not
/// have fails the whole statement, which empties the schema tree exactly as
/// CockroachDB's `pk_position` width did.
struct CatalogShape {
    column_query: ColumnQuery,
    /// Whether the index query may bound its key columns with `indnkeyatts`.
    bounded_index_keys: bool,
    /// Whether `pg_constraint` has `conindid` and `contype`, which say which
    /// indexes a constraint owns ([`pg_index_constraint`]). Every engine
    /// measured has both; one without them reports no constraint indexes
    /// rather than an empty schema tree.
    constraint_indexes: bool,
}

/// Asks the catalog which shape it is, once per introspection.
///
/// **Deliberately a probe rather than a try-and-fall-back.** Falling back on
/// any error conflates "this server lacks the column" with "that query
/// happened to fail", and the two want opposite handling: the first should
/// degrade, the second must not. A transient failure — YugabyteDB's
/// `MISMATCHED_SCHEMA` fires during introspection on roughly half of that
/// engine's test runs — would otherwise be swallowed by a retry in
/// the portable shape that *succeeds with wrong answers*: identity columns
/// reported as ordinary, enums stripped of their variants, arrays of their
/// structure. Silently, on an engine that has all three. A real error now
/// stays a real error.
///
/// Unknown counts as rich: it is what stock Postgres needs, it is the shape
/// this code has always used, and being wrong that way surfaces an error
/// rather than quietly wrong metadata.
async fn catalog_shape(pool: &PgPool) -> CatalogShape {
    // Asked of `pg_attribute` rather than of `information_schema.columns`,
    // even though the latter reads more naturally. The probe would then depend
    // on `information_schema` describing itself and `pg_catalog` — neither of
    // which the standard requires — and a server that declined would answer
    // zero, which reads as "this column is missing" rather than as "I don't
    // know". That is the one direction with no safe default: dropping the
    // `indnkeyatts` bound on an engine that *does* have INCLUDE columns would
    // silently promote payload columns to key columns, and a test run could
    // not tell, since the probe-failure default and the right answer coincide
    // on every engine that works. `pg_attribute` is already load-bearing for
    // introspection, so asking it costs no new assumption.
    let probe = sqlx::query_as::<_, (i64, i64, i64)>(
        "SELECT \
           (SELECT count(*) FROM pg_attribute a \
            JOIN pg_class c ON c.oid = a.attrelid \
            JOIN pg_namespace n ON n.oid = c.relnamespace \
            WHERE n.nspname = 'information_schema' AND c.relname = 'columns' \
              AND a.attname IN ('udt_schema', 'udt_name', \
                                'is_identity', 'identity_generation', 'is_generated')), \
           (SELECT count(*) FROM pg_attribute a \
            JOIN pg_class c ON c.oid = a.attrelid \
            JOIN pg_namespace n ON n.oid = c.relnamespace \
            WHERE n.nspname = 'pg_catalog' AND c.relname = 'pg_index' \
              AND a.attname = 'indnkeyatts'), \
           (SELECT count(*) FROM pg_attribute a \
            JOIN pg_class c ON c.oid = a.attrelid \
            JOIN pg_namespace n ON n.oid = c.relnamespace \
            WHERE n.nspname = 'pg_catalog' AND c.relname = 'pg_constraint' \
              AND a.attname IN ('conindid', 'contype'))",
    )
    .fetch_one(pool)
    .await;
    let (columns, index_keys, constraint_columns) = probe.unwrap_or((5, 1, 2));
    CatalogShape {
        // All five or none: the rich shape needs every one of them, and no
        // engine has been seen to offer a subset.
        column_query: if columns == 5 {
            ColumnQuery::Rich
        } else {
            ColumnQuery::Portable
        },
        bounded_index_keys: index_keys > 0,
        constraint_indexes: constraint_columns == 2,
    }
}

/// The index query introspection runs: one row per key column. See the
/// comment where it runs for the `indnkeyatts` bound and the `unnest` join.
///
/// `constraint_indexes` adds the type of the constraint owning each index,
/// for [`pg_index_constraint`]. Only `p`, `u` and `x` own their index: a
/// foreign key's `conindid` names the index on the *referenced* table it leans
/// on (the trap `fetch_ddl`'s index query documents), so it must not be
/// matched. Without those catalog columns the type is selected as a NULL, the
/// constant it would decode to for an unowned index, and `pg_constraint` is not
/// named at all.
///
/// **Joined once, never correlated.** `pg_constraint` has no index on
/// `conindid`, and the query has a row per key column, so a scalar subquery
/// per row scanned the whole of `pg_constraint` for each one — measured on
/// PostgreSQL 17 with 3,000 tables (12,000 rows, 6,000 constraints): 7 ms
/// without the column, 1,478 ms with a correlated subquery, 21 ms with this
/// pre-aggregated join. `GROUP BY` with `min` keeps the subquery's at-most-one
/// row per index without `DISTINCT ON`. Pinned by
/// `the_constraint_type_is_joined_once_not_looked_up_per_row`.
fn index_query(bound_keys: bool, constraint_indexes: bool) -> String {
    let (constraint_type, constraint_join) = if constraint_indexes {
        (
            "con.contype",
            "LEFT JOIN (SELECT conindid, min(contype::text) AS contype \
                        FROM pg_constraint WHERE contype IN ('p', 'u', 'x') \
                        GROUP BY conindid) con ON con.conindid = ix.indexrelid",
        )
    } else {
        ("NULL::text", "")
    };
    let bound = if bound_keys {
        "AND k.ord <= ix.indnkeyatts"
    } else {
        ""
    };
    format!(
        "SELECT n.nspname AS table_schema, t.relname AS table_name, \
                i.relname AS index_name, ix.indisunique AS is_unique, \
                ix.indpred IS NOT NULL AS is_partial, \
                {constraint_type} AS constraint_type, \
                k.ord AS key_position, a.attname AS column_name \
         FROM pg_index ix \
         JOIN pg_class t ON t.oid = ix.indrelid \
         JOIN pg_class i ON i.oid = ix.indexrelid \
         JOIN pg_namespace n ON n.oid = t.relnamespace \
         CROSS JOIN unnest(ix.indkey) WITH ORDINALITY AS k(attnum, ord) \
         LEFT JOIN pg_attribute a ON a.attrelid = t.oid AND a.attnum = k.attnum \
         {constraint_join} \
         WHERE n.nspname NOT IN ('pg_catalog', 'information_schema') \
           AND n.nspname NOT LIKE 'pg\\_%' \
           {bound} \
           AND ix.indisvalid \
         ORDER BY n.nspname, t.relname, i.relname, k.ord"
    )
}

/// The constraint that owns an index, from the `pg_constraint.contype` of the
/// constraint whose `conindid` names it.
///
/// Measured on every Postgres-wire engine: PostgreSQL, TimescaleDB, Citus and
/// YugabyteDB refuse `DROP INDEX` on a `p`, `u` or `x` constraint's index
/// ("cannot drop index … because constraint … requires it") and drop a
/// `CREATE [UNIQUE] INDEX` one. Materialize and RisingWave have no such
/// constraints and an empty `pg_constraint`.
///
/// **CockroachDB's `u` is not taken at its word.** It lists a plain `CREATE
/// UNIQUE INDEX` in `pg_constraint` as a `u` constraint too, with a `conindid`,
/// and nothing in `pg_constraint`, `information_schema.table_constraints`,
/// `SHOW CONSTRAINTS` or `pg_index` tells the two apart — yet it drops the
/// index and refuses the constraint's ("index … is in use as unique
/// constraint"). No catalog answers the question there, so the flavor decides
/// not to claim one: a unique index on CockroachDB is offered for Drop and the
/// engine's refusal is what the user sees for a constraint's. Its primary key
/// is unambiguous and is reported ("cannot drop the primary index").
pub(super) fn pg_index_constraint(
    contype: Option<&str>,
    flavor: PgFlavor,
) -> Option<IndexConstraint> {
    match contype? {
        "p" => Some(IndexConstraint::PrimaryKey),
        "u" if flavor == PgFlavor::CockroachDB => None,
        "u" => Some(IndexConstraint::Unique),
        "x" => Some(IndexConstraint::Exclusion),
        _ => None,
    }
}

/// Which shape of the column query to run — see [`column_query`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ColumnQuery {
    /// Everything stock Postgres exposes, including identity/generated flags
    /// and the `udt_*` pair that resolves enum and array structure.
    Rich,
    /// Only the columns every Postgres-wire `information_schema` has. The
    /// missing ones are selected as the constants they would decode to, so the
    /// result shape — and therefore the decoding below — is identical.
    ///
    /// What it gives up is real: no identity/generated classification, and no
    /// enum or array structure. On a server that lacks these columns all three
    /// are absent anyway, so the degraded answer is also the correct one —
    /// which is exactly why it must be chosen by [`catalog_shape`] asking, and
    /// never by a query having failed.
    Portable,
}

/// Where the column query reads primary-key positions from — see
/// [`pk_source`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PkSource {
    /// `information_schema.table_constraints` joined to `key_column_usage`:
    /// the standard's own answer, and what every engine but RisingWave is
    /// asked.
    InformationSchema,
    /// `pg_constraint.conkey`, one row per key column in key order, joined to
    /// `pg_attribute` for its name.
    PgConstraint,
}

/// Which catalog the column query asks for primary-key positions on `flavor`.
///
/// **RisingWave is asked `pg_constraint`, because its `information_schema`
/// cannot be read while a table is being dropped.** Its `table_constraints`
/// and `key_column_usage` views filter tables through
/// `has_table_privilege(conrelid, …)`, and that call *raises* — `XX000`,
/// "Invalid parameter oid: Table N not found" — for a table dropped after the
/// view's join found it. One such row fails the whole statement, and with it
/// introspection. Measured on RisingWave 3.1 with one session creating and
/// dropping a keyed table in a loop: 19 of 300 reads of `table_constraints`
/// failed, 22 of 300 of `key_column_usage`, and 0 of 500 of the
/// `pg_constraint` form; `information_schema.columns` and `tables` do not call
/// it and never failed. So a user browsing a RisingWave database while anyone
/// drops a table could lose the schema tree, and the test suite, whose tests
/// drop their fixtures while others introspect, failed now and then.
/// PostgreSQL itself is untouched by this: its `has_table_privilege` answers
/// NULL for an oid that has gone, and 3,000 reads of `table_constraints`
/// against 3,000 create/drop pairs failed none.
///
/// Measured on the same server, the `pg_constraint` form is also the right
/// answer where the views' is not: RisingWave's `key_column_usage` reports a
/// key column's *table* position as its `ordinal_position`, so a key declared
/// `PRIMARY KEY (c, a)` over columns `(a, b, c)` came back `a` = 1, `c` = 3,
/// where `conkey` says `c` first, then `a`. A materialized view's key had the
/// same fault and now reads in key order too, which changes only what is
/// shown: a view gets no row identity.
///
/// **What it costs: the key is no longer hidden by privilege.** The views
/// list a table's key only where `has_table_privilege(…, 'INSERT, UPDATE,
/// DELETE')` holds, so a `SELECT`-only user saw no key on RisingWave (as on
/// PostgreSQL, measured: a `SELECT`-only role sees none through the views
/// there either). `pg_constraint` shows it to everyone. On RisingWave that
/// gives no edit controls: every table there is refused for want of
/// transactions ([`UNGUARDED_WRITES`](super::caps::UNGUARDED_WRITES)) whatever
/// its key, so such a user's tables now say that instead of "no row identity",
/// and keyed reads (a long cell re-fetched by its row's key) work for them.
/// Filtering by privilege again was measured: `has_table_privilege` by oid or
/// by name raises on a dropped table just the same (36 and 34 of 500 reads),
/// and reading the privilege out of `rw_relations.acl` held up (0 of 500, and
/// no key for a `SELECT`-only user) but reimplements the engine's privilege
/// rules in SQL and drew RisingWave's "plan is too deep" warning on its own.
///
/// By flavor, because no catalog fact says a view raises: the flavor picks
/// which catalog to ask, and the answer still comes from the catalog. Only
/// RisingWave: the other engines answer the views without raising, and moving
/// them would change what they report — the standard views show a key only on
/// a table the user holds a privilege on — for no failure seen.
fn pk_source(flavor: PgFlavor) -> PkSource {
    match flavor {
        PgFlavor::RisingWave => PkSource::PgConstraint,
        PgFlavor::Postgres | PgFlavor::CockroachDB | PgFlavor::Yugabyte | PgFlavor::Materialize => {
            PkSource::InformationSchema
        }
    }
}

/// Columns for every relation, with primary-key positions resolved in SQL.
///
/// Identity and generated columns carry a NULL `column_default` even though
/// the database supplies their value, so `is_identity`/`identity_generation`/
/// `is_generated` are surfaced separately and mapped into
/// [`ColumnMeta::generated`] (required-column detection on row insert, and
/// read-only gating). Materialized-view columns are not in
/// `information_schema.columns` at all, so they come from `pg_attribute` via a
/// UNION — matviews have no PK, identity or generated columns, so those are
/// constant in that half, and `format_type` yields the type name with its
/// modifiers (e.g. `character varying(255)`). `ord` orders columns within a
/// relation across both halves.
///
/// `pk_position` is cast to `int8` rather than taken as it comes: the SQL
/// standard leaves `information_schema` positions as an implementation-defined
/// exact numeric, and CockroachDB makes `key_column_usage.ordinal_position`
/// 64-bit where stock Postgres makes it 32-bit. Decoding is exact by
/// wire type, so the mismatch failed the *whole* introspection. Pinning the
/// width costs nothing on Postgres and makes the decode independent of what any
/// engine chose.
fn column_query(shape: ColumnQuery, flavor: PgFlavor) -> String {
    // The three spans that differ. The portable shape must select the same
    // column names in the same order, so only the expressions change.
    let (generated, type_detail, type_join) = match shape {
        ColumnQuery::Rich => (
            "c.is_identity, c.identity_generation, c.is_generated",
            "ut.typtype::text AS typtype, ut.typcategory::text AS typcategory, \
             ut.oid::int8 AS type_oid, \
             c.udt_schema AS type_schema, c.udt_name AS type_base",
            "LEFT JOIN pg_namespace un ON un.nspname = c.udt_schema \
             LEFT JOIN pg_type ut ON ut.typname = c.udt_name AND ut.typnamespace = un.oid",
        ),
        ColumnQuery::Portable => (
            "'NO' AS is_identity, NULL::text AS identity_generation, 'NEVER' AS is_generated",
            "NULL::text AS typtype, NULL::text AS typcategory, \
             NULL::int8 AS type_oid, \
             NULL::text AS type_schema, NULL::text AS type_base",
            "",
        ),
    };
    // The key columns as (table_schema, table_name, column_name,
    // ordinal_position), the position being the column's place in the key.
    let keys = match pk_source(flavor) {
        PkSource::InformationSchema => {
            "SELECT kcu.table_schema, kcu.table_name, kcu.column_name, kcu.ordinal_position \
             FROM information_schema.table_constraints tc \
             JOIN information_schema.key_column_usage kcu \
               ON kcu.constraint_name = tc.constraint_name \
              AND kcu.constraint_schema = tc.constraint_schema \
              AND kcu.table_schema = tc.table_schema \
              AND kcu.table_name = tc.table_name \
             WHERE tc.constraint_type = 'PRIMARY KEY'"
        }
        PkSource::PgConstraint => {
            "SELECT n.nspname AS table_schema, t.relname AS table_name, \
                    a.attname AS column_name, k.ord AS ordinal_position \
             FROM pg_constraint con \
             JOIN pg_class t ON t.oid = con.conrelid \
             JOIN pg_namespace n ON n.oid = t.relnamespace \
             CROSS JOIN unnest(con.conkey) WITH ORDINALITY AS k(attnum, ord) \
             JOIN pg_attribute a ON a.attrelid = t.oid AND a.attnum = k.attnum \
             WHERE con.contype = 'p'"
        }
    };
    format!(
        "SELECT c.table_schema, c.table_name, c.column_name, c.data_type, \
                c.is_nullable, c.column_default, \
                {generated}, \
                pk.ordinal_position::int8 AS pk_position, \
                c.ordinal_position AS ord, \
                {type_detail} \
         FROM information_schema.columns c \
         {type_join} \
         LEFT JOIN ( {keys} ) pk ON pk.table_schema = c.table_schema \
             AND pk.table_name = c.table_name \
             AND pk.column_name = c.column_name \
         WHERE c.table_schema NOT IN ('pg_catalog', 'information_schema') \
         UNION ALL \
         SELECT n.nspname, c.relname, a.attname, \
                format_type(a.atttypid, a.atttypmod), \
                CASE WHEN a.attnotnull THEN 'NO' ELSE 'YES' END, \
                NULL::text, 'NO', NULL::text, 'NEVER', \
                NULL::int8, a.attnum::int, \
                t.typtype::text, t.typcategory::text, t.oid::int8, \
                tn.nspname, t.typname \
         FROM pg_attribute a \
         JOIN pg_class c ON c.oid = a.attrelid \
         JOIN pg_namespace n ON n.oid = c.relnamespace \
         JOIN pg_type t ON t.oid = a.atttypid \
         JOIN pg_namespace tn ON tn.oid = t.typnamespace \
         WHERE c.relkind = 'm' AND a.attnum > 0 AND NOT a.attisdropped \
           AND n.nspname NOT IN ('pg_catalog', 'information_schema') \
           AND NOT EXISTS ( \
             SELECT 1 FROM information_schema.columns ic \
             WHERE ic.table_schema = n.nspname AND ic.table_name = c.relname \
           ) \
         ORDER BY table_schema, table_name, ord"
    )
}

/// The text of `columns`, in order, from each row of a best-effort catalog
/// query that decodes — Citus's shards, Timescale's labels, Materialize's
/// reserved schemas.
///
/// Each of those queries already discards its own failure, because a badge or
/// a hidden schema is not worth failing a whole introspection over. Decoding
/// its rows is the same bet one step later: a NULL or a retyped column in a
/// future release would otherwise have returned from [`introspect`] through a
/// `?` and taken the schema tree with it, over a row the query was allowed to
/// lose. So a row that does not decode is skipped, and only that row.
///
/// Returning plain values rather than a `Result` is what makes that hold at
/// the call sites: there is nothing left for them to propagate. Generic over
/// the row type because a `PgRow` cannot be built without a server, and the
/// skipping is worth a test that runs everywhere.
fn best_effort_names<'r, R, const N: usize>(
    rows: &'r [R],
    columns: [&'r str; N],
) -> Vec<[String; N]>
where
    R: sqlx::Row,
    &'r str: sqlx::ColumnIndex<R>,
    String: sqlx::Decode<'r, R::Database> + sqlx::Type<R::Database>,
{
    rows.iter()
        .filter_map(|row| {
            let mut names: [String; N] = std::array::from_fn(|_| String::new());
            for (name, column) in names.iter_mut().zip(columns) {
                *name = row.try_get(column).ok()?;
            }
            Some(names)
        })
        .collect()
}

/// Full multi-schema introspection: every user schema's tables and views
/// with columns, primary keys, indexes (incl. unique), and foreign keys —
/// parity with the SQLite metadata model. A fixed number of batched queries
/// regardless of table count, in phases:
///
/// 1. [`internal_markings`]: what is the database's own rather than the
///    user's, and which of the extensions that change introspection are
///    installed ([`Installed`]);
/// 2. [`kind_labels`]: Timescale's names for its kinds of object;
/// 3. [`list_relations`]: the tables and views, each marked and labelled by
///    the two phases before — which take nothing from it, and it nothing from
///    them but their results;
/// 4. the column, index and foreign-key reads, attached to the tables by
///    [`attach_columns`], [`attach_indexes`] and [`attach_foreign_keys`].
///
/// Each phase is handed what it needs as a value, so none can read state a
/// later phase changes.
pub async fn introspect(conn: &PgConn) -> Result<Vec<TableMeta>, DbError> {
    let pool = conn.pool();
    let (marks, installed) = internal_markings(conn).await?;
    let labels = kind_labels(pool, installed).await;
    let mut tables = list_relations(pool, &marks, &labels).await?;

    // Columns with PK positions resolved in SQL (LEFT JOIN on the pkey
    // constraint), one row per column. Identity and generated columns have a
    // NULL column_default even though the database supplies their value, so
    // `is_identity`/`identity_generation`/`is_generated` are surfaced
    // separately and mapped into `ColumnMeta.generated` (required-column
    // detection on row insert, and read-only gating).
    // Materialized-view columns aren't in information_schema.columns either, so
    // they come from pg_catalog (pg_attribute) via a UNION. Matviews
    // have no PK, identity, or generated columns, so those are constant here;
    // `format_type` yields the type name (with modifiers, e.g.
    // `character varying(255)`). `ord` orders columns within a relation across
    // both halves of the UNION.
    //
    // `pk_position` is cast to `int8` rather than taken as it comes: the SQL
    // standard leaves `information_schema` positions as an implementation-
    // defined exact numeric, and CockroachDB makes
    // `key_column_usage.ordinal_position` a 64-bit integer where stock
    // Postgres makes it 32-bit. Decoding is exact by wire type, so
    // the mismatch failed the *whole* introspection — one column's width
    // taking down the entire schema tree. Pinning the width in SQL costs
    // nothing on Postgres and makes the decode independent of what any
    // Postgres-wire engine chose here.
    // Which of the optional catalog columns this server actually has.
    // Asked once, before anything depends on the answer.
    let shape = catalog_shape(pool).await;

    let column_rows = sqlx::query(&column_query(shape.column_query, conn.flavor()))
        .fetch_all(pool)
        .await
        .map_err(catalog_error)?;

    // Enum variants for every enum type in the database, keyed by type OID.
    // One query rather than per-column: enum types are few and a
    // type is typically shared by several columns. `enumsortorder` is
    // declaration order, which is the order the editor's dropdown offers.
    let enum_rows = sqlx::query(
        "SELECT enumtypid::int8 AS type_oid, enumlabel::text AS label \
         FROM pg_enum ORDER BY enumtypid, enumsortorder",
    )
    .fetch_all(pool)
    .await
    .map_err(catalog_error)?;
    let mut enum_variants: HashMap<i64, Vec<String>> = HashMap::new();
    for row in &enum_rows {
        let type_oid: i64 = get(row, "type_oid")?;
        let label: String = get(row, "label")?;
        enum_variants.entry(type_oid).or_default().push(label);
    }

    // Indexes from pg_catalog (information_schema has no index view).
    // Expression-index entries have a 0 attnum and no attribute row; those
    // key positions surface as NULL column names. Partial indexes
    // (indpred) are flagged so row-identity detection can reject them;
    // invalid indexes (e.g. from a failed CREATE INDEX CONCURRENTLY) make
    // no guarantees at all and are dropped entirely.
    // The `unnest` join carries no explicit `LATERAL`. PostgreSQL implies it
    // for a set-returning function in FROM, so the keyword adds nothing there,
    // and RisingWave's parser rejects it outright — it wants a subquery after
    // `LATERAL` and fails to prepare the statement, taking introspection with
    // it. Verified equivalent on every engine in this milestone.
    //
    // `indnkeyatts` separates an index's key columns from its INCLUDE payload,
    // and is absent on engines that have no INCLUDE to separate — Materialize
    // among them. Dropped from the query on those servers rather than
    // failing it. On an engine without INCLUDE that changes nothing; there is
    // no engine with INCLUDE that hides the count, and if one appeared its
    // payload columns would read as key columns.
    let index_rows = sqlx::query(&index_query(
        shape.bounded_index_keys,
        shape.constraint_indexes,
    ))
    .fetch_all(pool)
    .await
    .map_err(catalog_error)?;

    // Foreign keys from pg_constraint; conkey/confkey arrays preserve the
    // multi-column ordering that information_schema loses.
    let fk_rows = sqlx::query(
        "SELECT n.nspname AS table_schema, t.relname AS table_name, \
                rn.nspname AS ref_schema, rt.relname AS ref_table, \
                (SELECT array_agg(a.attname ORDER BY x.ord) \
                 FROM unnest(c.conkey) WITH ORDINALITY AS x(attnum, ord) \
                 JOIN pg_attribute a ON a.attrelid = c.conrelid AND a.attnum = x.attnum \
                ) AS columns, \
                (SELECT array_agg(a.attname ORDER BY x.ord) \
                 FROM unnest(c.confkey) WITH ORDINALITY AS x(attnum, ord) \
                 JOIN pg_attribute a ON a.attrelid = c.confrelid AND a.attnum = x.attnum \
                ) AS ref_columns \
         FROM pg_constraint c \
         JOIN pg_class t ON t.oid = c.conrelid \
         JOIN pg_namespace n ON n.oid = t.relnamespace \
         JOIN pg_class rt ON rt.oid = c.confrelid \
         JOIN pg_namespace rn ON rn.oid = rt.relnamespace \
         WHERE c.contype = 'f' \
           AND n.nspname NOT IN ('pg_catalog', 'information_schema') \
           AND n.nspname NOT LIKE 'pg\\_%' \
         ORDER BY n.nspname, t.relname, c.conname",
    )
    .fetch_all(pool)
    .await
    .map_err(catalog_error)?;

    let index = table_index(&tables);
    attach_columns(&mut tables, &index, &column_rows, &enum_variants)?;
    attach_indexes(&mut tables, &index, &index_rows, conn.flavor())?;
    attach_foreign_keys(&mut tables, &index, &fk_rows)?;
    Ok(tables)
}

/// What introspection marks as the database's own rather than the user's:
/// per schema, and per object — an object's own marking wins over its
/// schema's, the object being the more specific fact.
#[derive(Debug, Default, PartialEq)]
struct InternalMarks {
    schemas: HashMap<String, Internal>,
    objects: HashMap<(String, String), Internal>,
}

/// Which of the extensions that change what introspection asks are installed.
/// Read once, from what the extension query found ([`fold_internal_rows`]),
/// and handed on as a value: no marking added afterwards — a Citus shard, a
/// Materialize schema — can change it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Installed {
    citus: bool,
    timescale: bool,
}

/// Phase 1 of [`introspect`]: the extension and partition markings from the
/// catalog, then each engine's own additions to them — Citus's shards,
/// Materialize's and RisingWave's reserved schemas. Returns the markings and
/// which extensions are installed, the latter decided before any addition.
async fn internal_markings(conn: &PgConn) -> Result<(InternalMarks, Installed), DbError> {
    let pool = conn.pool();
    // Objects that are the database's own bookkeeping rather than the user's
    // data, from the three sources Postgres has. All three are
    // catalog facts, never name patterns — a user table genuinely called
    // `spatial_ref_sys` is the user's.
    //
    //  1. Schemas an extension created. `pg_depend` records the namespace →
    //     extension dependency, so this catches TimescaleDB's seven schemas,
    //     PostGIS's `tiger`/`topology` and Citus's catalogs by construction.
    //     A schema the *user* made has no such dependency even when an
    //     extension puts objects in it — which is the distinction that
    //     matters, since `public` holds PostGIS's functions and is still the
    //     user's schema.
    //  2. Individual objects an extension installs into an ordinary schema:
    //     PostGIS's `spatial_ref_sys`, `pg_stat_statements`'s view. Same
    //     catalog, keyed on `pg_class` instead of `pg_namespace`.
    //  3. Child partitions of declaratively partitioned tables. Not an
    //     extension matter at all, but a table partitioned by day floods the
    //     tree exactly as Timescale's chunks do.
    //
    // The placeholder is `NULL::text` rather than the `name` that would match
    // its sibling columns: `name` is a PostgreSQL-internal type, and an engine
    // reimplementing the catalog need not have it — RisingWave does not, and
    // failed to bind the cast at all, taking the whole introspection with it.
    // Text unions with `name` on every engine that has both, and the
    // value is decoded as a string either way.
    //
    // `deptype = 'e'` on the first two: that is the code for "member of the
    // extension". The neighbouring `'x'` means the opposite — the object is
    // *not* a member, it merely gets dropped with the extension — so counting
    // it would attribute the user's own object to an extension.
    let internal_rows = sqlx::query(
        "SELECT n.nspname AS schema_name, NULL::text AS object_name, e.extname \
         FROM pg_namespace n \
         JOIN pg_depend d ON d.classid = 'pg_namespace'::regclass AND d.objid = n.oid \
          AND d.deptype = 'e' \
         JOIN pg_extension e ON e.oid = d.refobjid \
          AND d.refclassid = 'pg_extension'::regclass \
         UNION ALL \
         SELECT n.nspname, c.relname, e.extname \
         FROM pg_class c \
         JOIN pg_namespace n ON n.oid = c.relnamespace \
         JOIN pg_depend d ON d.classid = 'pg_class'::regclass AND d.objid = c.oid \
          AND d.deptype = 'e' \
         JOIN pg_extension e ON e.oid = d.refobjid \
          AND d.refclassid = 'pg_extension'::regclass \
         UNION ALL \
         SELECT n.nspname, c.relname, NULL \
         FROM pg_class c \
         JOIN pg_namespace n ON n.oid = c.relnamespace \
         WHERE c.relispartition",
    )
    .fetch_all(pool)
    .await
    .map_err(catalog_error)?;
    let mut rows = Vec::with_capacity(internal_rows.len());
    for row in &internal_rows {
        rows.push((
            get::<String, _>(row, "schema_name")?,
            get::<Option<String>, _>(row, "object_name")?,
            get::<Option<String>, _>(row, "extname")?,
        ));
    }
    let (mut marks, installed) = fold_internal_rows(rows);

    // Citus shard tables. Citus normally keeps these out of
    // `pg_class` for client queries itself, so introspection never sees them
    // and this finds nothing — but that hiding is a *setting*
    // (`citus.show_shards_for_app_name_prefixes`), and with it widened a
    // 26-table database reports 290, of which 264 are shards. They are not
    // extension members and not partitions, so nothing else here catches
    // them.
    //
    // `pg_dist_shard` is the catalog Citus itself uses, and `shard_name()` is
    // Citus's own function for naming a row of it — asking beats reproducing
    // the rule, which is not the plain `<table>_<shardid>` it looks like:
    // Citus hashes and truncates once that would exceed NAMEDATALEN, so a
    // 63-character table shards into `..._01ae35b2_102008`.
    //
    // Its answer is then resolved back to a real `pg_class` row rather than
    // used as a name, because `shard_name()` qualifies anything outside
    // `public` — a shard in a `sales` schema comes back as
    // `sales.orders_102008`, which matches no `relname` anywhere. Resolving
    // it yields the schema and the bare name the caller needs, and means no
    // string is ever parsed.
    //
    // Two things that resolution has to be pinned against:
    //
    // `to_regclass` rather than a `::regclass` cast, because the cast *raises*
    // on a name it cannot resolve, and a multi-node coordinator's
    // `pg_dist_shard` lists shards living on workers with no local relation.
    // The cast would error, the `if let Ok` below would swallow it, and
    // nothing at all would be marked.
    //
    // `sc.relnamespace = c.relnamespace`, because `to_regclass` resolves
    // through the search path: a user table named `decoy.pub_102008` with
    // `decoy` ahead of `public` otherwise resolves *instead of* the real
    // shard, and the user's own table is what gets hidden while the shard
    // stays visible — backwards on both counts, and not gated behind the
    // visibility setting, since name resolution goes through the catalog
    // cache rather than the `pg_class` scan Citus filters. Shards always live
    // in their table's schema, so pinning the namespace costs nothing.
    //
    // Residual, and benign: a `search_path` that excludes `public` leaves
    // `public` shards unresolvable, so they go unmarked — the same place they
    // would be if Citus were hiding them, which by default it is.
    // Best-effort for the same reason as the Timescale labels ([`kind_labels`]).
    if installed.citus {
        let shards = sqlx::query(
            "SELECT sn.nspname AS schema_name, sc.relname AS object_name \
             FROM pg_dist_shard s \
             JOIN pg_class c ON c.oid = s.logicalrelid \
             JOIN pg_class sc \
               ON sc.oid = pg_catalog.to_regclass( \
                    pg_catalog.shard_name(s.logicalrelid, s.shardid)) \
              AND sc.relnamespace = c.relnamespace \
             JOIN pg_namespace sn ON sn.oid = sc.relnamespace",
        )
        .fetch_all(pool)
        .await;
        if let Ok(rows) = shards {
            for [schema, object] in best_effort_names(&rows, ["schema_name", "object_name"]) {
                // Same precedence as the branches above: naming the extension
                // beats naming the shape, so a shard that is also a partition
                // reports the extension rather than disagreeing with them.
                marks
                    .objects
                    .insert((schema, object), Internal::Extension("citus".to_string()));
            }
        }
    }

    // Schemas Materialize reserves for its own catalog. It exposes
    // five beyond the two [`list_relations`] excludes — `mz_catalog`, `mz_internal`,
    // `mz_introspection`, `mz_unsafe`, `mz_catalog_unstable` — holding 265
    // objects between them, which would otherwise be the overwhelming majority
    // of the schema tree.
    //
    // No cross-engine catalog fact reaches these. Unlike CockroachDB's they are
    // reported as ordinary tables and views rather than `SYSTEM VIEW`, and the
    // extension path above finds nothing: Materialize's `pg_depend` has rows,
    // but none with `deptype = 'e'`, because it has no extensions — these
    // schemas are the engine itself.
    //
    // Materialize does record it, in the one place only Materialize has.
    // `mz_schemas.database_id` is null exactly for the schemas that belong to
    // no database, which is what a system schema is; the `s`/`u` prefix on
    // `mz_schemas.id` says the same thing, but it is an id encoding rather than
    // a documented column contract and its representation has changed before.
    //
    // So this is the case `PgFlavor` exists for: knowing the engine is what
    // tells us *which* catalog to ask, and the answer still comes from the
    // catalog rather than from a list of names.
    //
    // Best-effort, like the Citus query above and the Timescale labels: a schema tree
    // cluttered with Materialize's internals is worse than one without the
    // badge, but neither is worth failing the whole introspection over.
    if conn.flavor() == PgFlavor::Materialize {
        if let Ok(rows) = sqlx::query("SELECT name FROM mz_schemas WHERE database_id IS NULL")
            .fetch_all(pool)
            .await
        {
            for [schema] in best_effort_names(&rows, ["name"]) {
                marks.schemas.insert(schema, Internal::System);
            }
        }
    }

    // RisingWave's own catalog. `rw_catalog` holds 74 objects and is
    // listed like any user schema, so without this it is most of the schema
    // tree.
    //
    // Not reachable by catalog fact alone, and the near-miss is worth
    // recording: 61 of the 74 report `table_type = 'SYSTEM TABLE'`, which the
    // classification in [`relation_meta`] picks up and which no other engine emits
    // outside `pg_catalog`. The remaining 13 are plain `VIEW`, indistinguishable
    // from a user's own. So the schema is what identifies them.
    //
    // Matching the name is sound here on the same two grounds as
    // `pg_catalog`: the connection has already identified itself as
    // RisingWave, so no other server can reach this; and on RisingWave
    // `rw_catalog` is reserved — `CREATE SCHEMA rw_catalog` is refused because
    // it already exists — so no user schema can ever be caught by it.
    if conn.flavor() == PgFlavor::RisingWave {
        marks
            .schemas
            .insert("rw_catalog".to_string(), Internal::System);
    }

    Ok((marks, installed))
}

/// The extension query's rows — `(schema, object, extension)`, an object of
/// `None` marking the whole schema and an extension of `None` a partition —
/// as markings, and which extensions they show installed. Over plain data so
/// a test can execute the precedence.
fn fold_internal_rows(
    rows: Vec<(String, Option<String>, Option<String>)>,
) -> (InternalMarks, Installed) {
    // Schema-wide entries and per-object entries are kept apart so an object
    // rule can win over its schema's — the object is the more specific fact.
    let mut internal_schemas: HashMap<String, Internal> = HashMap::new();
    let mut internal_objects: HashMap<(String, String), Internal> = HashMap::new();
    for (schema, object, extension) in rows {
        let reason = match extension {
            Some(extension) => Internal::Extension(extension),
            None => Internal::Partition,
        };
        match object {
            // An object can qualify twice — a partition an extension owns
            // hits branches 2 and 3 — and the UNION has no defined row order,
            // so name the winner rather than letting it be whichever row
            // arrived last. Naming the extension is the more useful of the
            // two, since "which extension" is the part the user can act on.
            Some(object) => match internal_objects.entry((schema, object)) {
                Entry::Occupied(mut slot) => {
                    if matches!(reason, Internal::Extension(_)) {
                        slot.insert(reason);
                    }
                }
                Entry::Vacant(slot) => {
                    slot.insert(reason);
                }
            },
            None => {
                internal_schemas.insert(schema, reason);
            }
        }
    }

    let marks = InternalMarks {
        schemas: internal_schemas,
        objects: internal_objects,
    };
    let has_extension = |name: &str| {
        marks
            .schemas
            .values()
            .chain(marks.objects.values())
            .any(|reason| matches!(reason, Internal::Extension(ext) if ext == name))
    };
    let installed = Installed {
        citus: has_extension("citus"),
        timescale: has_extension("timescaledb"),
    };
    (marks, installed)
}

/// Phase 2 of [`introspect`]: Timescale's labels for its objects, keyed by
/// (schema, name). Empty unless Timescale is installed.
async fn kind_labels(pool: &PgPool, installed: Installed) -> HashMap<(String, String), String> {
    // Timescale's own vocabulary for objects that are otherwise ordinary
    // tables and views, so a hypertable reads as one instead of
    // looking like a table that mysteriously has chunks. Best-effort: these
    // information views are stable across Timescale 2.x, but a badge is not
    // worth failing a whole introspection over if a future version moves
    // them, and the extension is absent on nearly every Postgres database.
    let mut kind_labels: HashMap<(String, String), String> = HashMap::new();
    if installed.timescale {
        let labelled = [
            (
                "SELECT hypertable_schema AS s, hypertable_name AS n \
                 FROM timescaledb_information.hypertables",
                "hypertable",
            ),
            (
                "SELECT view_schema AS s, view_name AS n \
                 FROM timescaledb_information.continuous_aggregates",
                "continuous aggregate",
            ),
        ];
        for (sql, label) in labelled {
            let Ok(rows) = sqlx::query(sql).fetch_all(pool).await else {
                continue;
            };
            for [schema, name] in best_effort_names(&rows, ["s", "n"]) {
                kind_labels.insert((schema, name), label.to_string());
            }
        }
    }

    kind_labels
}

/// Phase 3 of [`introspect`]: every table and view, marked internal and
/// labelled from the earlier phases' results, with no columns, indexes or
/// foreign keys yet.
async fn list_relations(
    pool: &PgPool,
    marks: &InternalMarks,
    labels: &HashMap<(String, String), String>,
) -> Result<Vec<TableMeta>, DbError> {
    // Tables and views across all non-system schemas. Materialized views
    // (relkind 'm') are not in stock Postgres's information_schema, so they
    // come from a pg_catalog UNION.
    //
    // "Not in information_schema" is a PostgreSQL choice, not a rule: Materialize
    // lists its materialized views there *and* reports `relkind = 'm'`, so both
    // halves of the UNION claim them and every such object arrived twice —
    // once whole, once as a duplicate that later grouping left empty.
    // The second half therefore takes only what the first did not, which is a
    // no-op on any server that behaves like stock Postgres.
    //
    // Temporary tables are left out, and this is the one place that decides
    // it: columns, indexes and foreign keys attach only to a table listed
    // here. A temp table belongs to the session that made it, and hubro's
    // sessions are a pool with no affinity — each query below, and every
    // later browse, lands on whichever connection is idle. Measured against
    // PostgreSQL 17 with a temp table made through the pool: listed by one
    // introspection and not the next, listed with its columns but not its
    // unique index (that query had already landed elsewhere), and, once the
    // pool had grown, listed with no columns at all while every attempt to
    // open it failed with "cannot access temporary tables of other sessions".
    //
    // One filter on this query is enough, because each engine that has temp
    // tables at all either hides the ones it would otherwise get wrong or
    // labels them. Measured with a temp table held open by a second session:
    // PostgreSQL leaves other sessions' temp schemas out of
    // `information_schema` entirely (they are still in `pg_class`), so the
    // only temp table it can list is the introspecting connection's own —
    // labelled `LOCAL TEMPORARY`. CockroachDB lists *every* session's temp
    // tables, and labels each one the same way. Materialize lists no temp
    // table or view at all, not even to the session that made it. Being the
    // server's own classification, the label needs no schema-name pattern;
    // the index and foreign-key queries' `pg\_%` filter excludes the same
    // schemas, but cannot be applied here without also dropping
    // CockroachDB's `pg_extension`, which is listed and marked internal.
    let table_rows = sqlx::query(
        "SELECT table_schema, table_name, table_type \
         FROM information_schema.tables \
         WHERE table_schema NOT IN ('pg_catalog', 'information_schema') \
           AND table_type <> 'LOCAL TEMPORARY' \
         UNION ALL \
         SELECT n.nspname, c.relname, 'MATERIALIZED VIEW' \
         FROM pg_class c \
         JOIN pg_namespace n ON n.oid = c.relnamespace \
         WHERE c.relkind = 'm' \
           AND n.nspname NOT IN ('pg_catalog', 'information_schema') \
           AND NOT EXISTS ( \
             SELECT 1 FROM information_schema.tables it \
             WHERE it.table_schema = n.nspname AND it.table_name = c.relname \
           ) \
         ORDER BY table_schema, table_name",
    )
    .fetch_all(pool)
    .await
    .map_err(catalog_error)?;

    // Tables whose writes a `DO INSTEAD` rule replaces (see [`INSTEAD_RULE`]).
    // Views are left out: every one carries an instead rule, `_RETURN`, and
    // is refused as a view already. Not best-effort like the extension
    // queries above: a failure here would leave such a table editable, and
    // every engine measured has `pg_rewrite` — CockroachDB, Materialize and
    // RisingWave with no rules in it, since none of them can create one.
    let rule_rows = sqlx::query(
        "SELECT DISTINCT n.nspname AS table_schema, c.relname AS table_name \
         FROM pg_rewrite r \
         JOIN pg_class c ON c.oid = r.ev_class \
         JOIN pg_namespace n ON n.oid = c.relnamespace \
         WHERE r.is_instead AND c.relkind NOT IN ('v', 'm')",
    )
    .fetch_all(pool)
    .await
    .map_err(catalog_error)?;
    let mut instead_rules = HashSet::with_capacity(rule_rows.len());
    for row in &rule_rows {
        instead_rules.insert((
            get::<String, _>(row, "table_schema")?,
            get::<String, _>(row, "table_name")?,
        ));
    }

    let mut tables = Vec::with_capacity(table_rows.len());
    for row in &table_rows {
        tables.push(relation_meta(
            get(row, "table_schema")?,
            get(row, "table_name")?,
            &get::<String, _>(row, "table_type")?,
            marks,
            labels,
            &instead_rules,
        ));
    }
    Ok(tables)
}

/// One listed relation as a [`TableMeta`] with nothing attached yet. Over
/// plain data so a test can execute it.
fn relation_meta(
    schema: String,
    name: String,
    table_type: &str,
    marks: &InternalMarks,
    labels: &HashMap<(String, String), String>,
    instead_rules: &HashSet<(String, String)>,
) -> TableMeta {
    let key = (schema.clone(), name.clone());
    // `SYSTEM VIEW` is the engine's own catalog, in a schema it reserves
    // beyond the two [`list_relations`] excludes. CockroachDB reports 119 such
    // objects — `crdb_internal` and `pg_extension` — and lists them here
    // exactly as it lists the user's tables, so without this the schema
    // tree is mostly Cockroach's bookkeeping, and most of it does not even
    // open (`crdb_internal` refuses to be read at all without
    // `allow_unsafe_internals`).
    //
    // Taken from `table_type` rather than from a list of schema names,
    // which the catalog-facts-not-names rule for internal objects rules
    // out: this is the server's own
    // classification of the object, in a query already being run. Stock
    // PostgreSQL never emits the value — it has nothing left to classify
    // once `pg_catalog` and `information_schema` are excluded — so the
    // rule needs no engine check and costs nothing to carry.
    let system = matches!(table_type, "SYSTEM VIEW" | "SYSTEM TABLE");
    // A streaming engine's edges, on both Materialize and
    // RisingWave. A source is continuously written by the engine
    // from somewhere else — Kafka, Postgres replication, a load generator
    // — so it is readable and never writable by hand, which is a view's
    // contract rather than a table's. A sink is the mirror image: it
    // *writes* to an external system and has no rows of its own at all.
    let source = table_type == "SOURCE";
    let sink = table_type == "SINK";
    // The object's own rule first, then its schema's — most specific wins.
    // Naming the extension beats naming the engine for the same reason it
    // beats naming the shape in [`fold_internal_rows`]: it is the part the
    // user can act on.
    let internal = marks
        .objects
        .get(&key)
        .cloned()
        .or_else(|| system.then_some(Internal::System))
        .or_else(|| marks.schemas.get(&schema).cloned());
    let kind_label = labels.get(&key).cloned().or_else(|| {
        // Materialize's own vocabulary for the object kind that has no
        // Postgres equivalent, so a source reads as one instead of
        // as a mysteriously unwritable view. Same treatment `hypertable`
        // and `continuous aggregate` get on Timescale: the label
        // refines the kind rather than replacing it.
        if source {
            Some("source".to_string())
        } else if sink {
            Some("sink".to_string())
        } else {
            None
        }
    });
    TableMeta {
        schema: Some(schema),
        name,
        kind: match table_type {
            // A `SYSTEM VIEW` is a view, and saying so keeps it read-only
            // by the ordinary route: nothing about being the engine's own
            // catalog makes its rows addressable, and the alternative
            // (falling through to `Table`) would offer editing on 119
            // objects that mostly cannot even be read. A Materialize
            // `SOURCE` is a view for the same reason — derived, readable,
            // never written by the user.
            "VIEW" | "SYSTEM VIEW" | "SOURCE" | "SINK" => TableKind::View,
            "MATERIALIZED VIEW" => TableKind::MaterializedView,
            _ => TableKind::Table,
        },
        columns: Vec::new(),
        indexes: Vec::new(),
        foreign_keys: Vec::new(),
        // Per-object narrowing only where the driver knows something the
        // resolver cannot derive. Kind and row identity carry the
        // rest, so this is `None` for everything but a streaming engine's
        // two edges and a table whose writes a `DO INSTEAD` rule
        // replaces. A *source* resolves to a view and would otherwise be
        // refused with "Views are read-only", a sentence that sends the
        // reader looking for a view definition that does not exist; a
        // *sink* has no rows at all, which nothing about its kind says.
        restriction: if source {
            Some(Restriction::Declared(STREAMING_SOURCE))
        } else if sink {
            // `NoRows`, not `Declared`: a sink stores nothing, so this one
            // declaration has to stop it being *opened* as well as edited.
            // Not gated on the flavor — `table_type = 'SINK'`
            // is the server's own classification, so any engine reporting
            // it inherits this.
            Some(Restriction::NoRows(STREAMING_SINK))
        } else if instead_rules.contains(&key) {
            Some(INSTEAD_RULE)
        } else {
            None
        },
        internal,
        kind_label,
    }
}

/// Phase 4 of [`introspect`]: each column row onto its table, by
/// [`table_index`]; a row whose table was not listed is dropped.
fn attach_columns(
    tables: &mut [TableMeta],
    table_index: &HashMap<(String, String), usize>,
    rows: &[PgRow],
    enum_variants: &HashMap<i64, Vec<String>>,
) -> Result<(), DbError> {
    for row in rows {
        let schema: String = get(row, "table_schema")?;
        let table: String = get(row, "table_name")?;
        let Some(&idx) = table_index.get(&(schema, table)) else {
            continue;
        };
        let nullable: String = get(row, "is_nullable")?;
        let pk_position: Option<i64> = get(row, "pk_position")?;
        // Both `GENERATED ALWAYS AS IDENTITY` and `GENERATED ALWAYS AS
        // (expr) STORED` are database-assigned and not writable, so both map
        // to `Generated::Always`; `GENERATED BY DEFAULT AS IDENTITY` is
        // overridable. `serial` is not generated — it carries a real
        // `nextval(…)` default and stays `Never`.
        let is_identity: String = get(row, "is_identity")?;
        let identity_generation: Option<String> = get(row, "identity_generation")?;
        let is_generated: String = get(row, "is_generated")?;
        let generated = if is_generated == "ALWAYS"
            || (is_identity == "YES" && identity_generation.as_deref() == Some("ALWAYS"))
        {
            Generated::Always
        } else if is_identity == "YES" {
            Generated::ByDefault
        } else {
            Generated::Never
        };
        // `data_type` is opaque for these two ('USER-DEFINED', 'ARRAY'), so
        // the editor's structure comes from pg_type instead.
        let typtype: Option<String> = get(row, "typtype")?;
        let typcategory: Option<String> = get(row, "typcategory")?;
        let type_oid: Option<i64> = get(row, "type_oid")?;
        let type_schema: Option<String> = get(row, "type_schema")?;
        let type_base: Option<String> = get(row, "type_base")?;
        let type_ref = match (type_schema, type_base) {
            (Some(schema), Some(name)) => Some(TypeRef { schema, name }),
            _ => None,
        };
        let type_detail = match (typtype.as_deref(), typcategory.as_deref(), type_ref) {
            (Some("e"), _, Some(type_ref)) => type_oid
                .and_then(|oid| enum_variants.get(&oid))
                .map(|variants| TypeDetail::Enum {
                    type_ref,
                    variants: variants.clone(),
                })
                .unwrap_or_default(),
            (_, Some("A"), Some(type_ref)) => TypeDetail::Array { type_ref },
            _ => TypeDetail::Plain,
        };
        tables[idx].columns.push(ColumnMeta {
            name: get(row, "column_name")?,
            type_name: get(row, "data_type")?,
            nullable: nullable == "YES",
            primary_key_position: pk_position.map(|p| p as u32),
            default: get::<Option<String>, _>(row, "column_default")?,
            generated,
            type_detail,
        });
    }

    Ok(())
}

/// Each index row onto its table, consecutive rows of one index becoming its
/// columns in order ([`super::schema::push_index_row`]).
fn attach_indexes(
    tables: &mut [TableMeta],
    table_index: &HashMap<(String, String), usize>,
    rows: &[PgRow],
    flavor: PgFlavor,
) -> Result<(), DbError> {
    for row in rows {
        let schema: String = get(row, "table_schema")?;
        let table: String = get(row, "table_name")?;
        let Some(&idx) = table_index.get(&(schema, table)) else {
            continue;
        };
        let index_name: String = get(row, "index_name")?;
        let column: Option<String> = get(row, "column_name")?;
        let column = column.unwrap_or_else(|| "<expr>".to_string());
        let unique: bool = get(row, "is_unique")?;
        let partial: bool = get(row, "is_partial")?;
        let contype: Option<String> = get(row, "constraint_type")?;
        push_index_row(
            &mut tables[idx].indexes,
            IndexMeta {
                name: index_name,
                unique,
                partial,
                columns: vec![column],
                constraint: pg_index_constraint(contype.as_deref(), flavor),
            },
        );
    }

    Ok(())
}

/// Each foreign-key row onto its table — one row per constraint, its columns
/// already aggregated in key order.
fn attach_foreign_keys(
    tables: &mut [TableMeta],
    table_index: &HashMap<(String, String), usize>,
    rows: &[PgRow],
) -> Result<(), DbError> {
    for row in rows {
        let schema: String = get(row, "table_schema")?;
        let table: String = get(row, "table_name")?;
        let Some(&idx) = table_index.get(&(schema, table)) else {
            continue;
        };
        let ref_columns: Vec<String> = get(row, "ref_columns")?;
        tables[idx].foreign_keys.push(ForeignKeyMeta {
            columns: get(row, "columns")?,
            referenced_schema: Some(get(row, "ref_schema")?),
            referenced_table: get(row, "ref_table")?,
            referenced_columns: ref_columns.into_iter().map(Some).collect(),
        });
    }

    Ok(())
}

/// DDL for one object.
///
/// Postgres renders view and index definitions itself (`pg_get_viewdef`,
/// `pg_get_indexdef`), and those come back verbatim. It has no `CREATE TABLE`
/// generator at all — `pg_dump` assembles that text in client code — so a
/// table is rebuilt from the catalog here.
///
/// The rebuild deliberately re-reads `pg_attribute`/`pg_attrdef`/`pg_constraint`
/// instead of leaning on the browsable [`TableMeta`]: introspection takes column
/// types from `information_schema.columns.data_type`, which reports `varchar(20)`
/// as a bare `character varying` and every user-defined type as `USER-DEFINED`.
/// Reconstructing from that would emit column types that are quietly *wrong*,
/// which is worse than not offering the feature. `pg_get_constraintdef`
/// likewise supplies check constraints and referential actions that the
/// browsable metadata does not carry at all, and `pg_get_expr(adbin, adrelid)`
/// supplies both the column defaults and the generation expressions — the same
/// call `information_schema.columns.column_default` is built from, read here so
/// one source covers both.
pub async fn fetch_ddl(
    pool: &PgPool,
    table: &TableMeta,
    object: &DdlObject,
) -> Result<Ddl, DbError> {
    // Postgres always qualifies; `public` is only a defensive default for a
    // TableMeta that somehow lost its schema.
    let schema = table.schema.clone().unwrap_or_else(|| "public".into());
    let params = [Value::Text(schema.clone())];

    if let DdlObject::Index(name) = object {
        let rows = query_with(
            pool,
            "SELECT pg_get_indexdef(c.oid) AS def \
             FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace \
             WHERE n.nspname::text = $1 AND c.relname::text = $2 AND c.relkind IN ('i', 'I')",
            &[params[0].clone(), Value::Text(name.clone())],
        )
        .await?;
        let Some(def) = rows.rows.first().and_then(|r| row_opt_text(r, 0)) else {
            return Err(DbError::Introspect(format!(
                "no index named {name} in {schema}"
            )));
        };
        return Ok(Ddl::native(terminate(&def, Dialect::Postgres)));
    }

    let params = [params[0].clone(), Value::Text(table.name.clone())];
    if matches!(table.kind, TableKind::View | TableKind::MaterializedView) {
        let rows = query_with(
            pool,
            "SELECT pg_get_viewdef(c.oid, true) AS def \
             FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace \
             WHERE n.nspname::text = $1 AND c.relname::text = $2",
            &params,
        )
        .await?;
        let Some(body) = rows.rows.first().and_then(|r| row_opt_text(r, 0)) else {
            return Err(DbError::Introspect(format!(
                "no view definition for {schema}.{}",
                table.name
            )));
        };
        // Only the CREATE header is ours; the body is the server's own text,
        // so this stays native (no "reconstructed" warning).
        return Ok(Ddl::native(create_view_sql(
            Dialect::Postgres,
            table,
            &body,
        )));
    }

    // A catalog read that fails still yields usable output: the renderer falls
    // back to what TableMeta knows and the failure becomes a visible caveat,
    // rather than the whole action erroring out. The standing caveats are
    // *extended*, never replaced — the degraded output must not claim to
    // reproduce more than the good one.
    let extras = match table_ddl_extras(pool, &params).await {
        Ok(extras) => extras,
        Err(err) => {
            let mut caveats = pg_standing_caveats();
            caveats.push(format!(
                "column types, defaults, collations, constraints and indexes — reading the \
                 catalog failed ({})",
                err.message()
            ));
            TableExtras {
                caveats,
                ..TableExtras::default()
            }
        }
    };
    Ok(create_table_sql(Dialect::Postgres, table, &extras))
}

/// Which storage statistics this server can be asked for.
///
/// The same probe-don't-guess discipline as [`catalog_shape`], for the same
/// reason: `pg_class` is the one catalog every Postgres-wire engine claims to
/// have and none of them implements fully. Of the seven engines in the test
/// matrix, three answer nothing usable — CockroachDB has the column but leaves
/// `reltuples` NULL and ships no `pg_total_relation_size` (its `crdb_internal`
/// equivalent is refused outright); Materialize has `reltuples` but reports -1
/// for everything and has no size function; RisingWave has neither.
///
/// Where it differs from [`catalog_shape`] is the default when the *probe*
/// cannot be run at all, and deliberately: an unknown catalog shape falls back
/// to the rich query because degrading there would report quietly wrong
/// metadata, whereas degrading here loses a number and nothing else. So a probe
/// this cannot answer means "no statistics", never "try it and see" — and a
/// probe that fails outright is propagated rather than read as an absence, so a
/// broken connection is not reported to the user as a server without statistics.
struct StatsShape {
    /// Whether `pg_class` has `reltuples`.
    row_estimate: bool,
    /// Whether `pg_total_relation_size(oid)` exists.
    total_size: bool,
}

impl StatsShape {
    /// Nothing to ask for, so there is no query worth sending.
    fn is_empty(&self) -> bool {
        !self.row_estimate && !self.total_size
    }
}

/// Asks the catalog which statistics it keeps, once per stats fetch.
///
/// Asked of `pg_attribute`/`pg_proc` rather than by running the real query and
/// catching the failure, so "this server has no such column" stays
/// distinguishable from "that query happened to fail".
async fn stats_shape(pool: &PgPool) -> Result<StatsShape, DbError> {
    let (columns, size_fn) = sqlx::query_as::<_, (i64, i64)>(
        "SELECT \
           (SELECT count(*) FROM pg_attribute a \
            JOIN pg_class c ON c.oid = a.attrelid \
            JOIN pg_namespace n ON n.oid = c.relnamespace \
            WHERE n.nspname = 'pg_catalog' AND c.relname = 'pg_class' \
              AND a.attname = 'reltuples'), \
           (SELECT count(*) FROM pg_proc p \
            JOIN pg_namespace n ON n.oid = p.pronamespace \
            WHERE n.nspname = 'pg_catalog' AND p.proname = 'pg_total_relation_size')",
    )
    .fetch_one(pool)
    .await
    .map_err(catalog_error)?;
    Ok(StatsShape {
        row_estimate: columns > 0,
        total_size: size_fn > 0,
    })
}

/// Row count and on-disk size for one table, from statistics the server already
/// keeps. Never scans: the row half is the planner's estimate, and an
/// exact number is a separate, explicit `COUNT(*)`.
///
/// Anything the server does not keep comes back as `None` rather than as a
/// zero, which is why the query selects a literal NULL for the halves
/// [`stats_shape`] says are missing instead of omitting them — the result shape,
/// and so the decoding, is the same either way.
pub async fn fetch_table_stats(pool: &PgPool, table: &TableMeta) -> Result<TableStats, DbError> {
    let shape = stats_shape(pool).await?;
    if shape.is_empty() {
        return Ok(TableStats::default());
    }
    let reltuples = match shape.row_estimate {
        true => "c.reltuples::float8",
        false => "NULL::float8",
    };
    // A plain view has no storage of its own, and `pg_total_relation_size`
    // answers 0 for one — which would render as a measured "0 B" rather than as
    // the absence it is. Read off `relkind` rather than off [`TableKind`]: a
    // materialized view has storage and must keep reporting it, and relkind is
    // the catalog's own answer to which is which. SQL Server's indexed views
    // reach the same conclusion from the other direction — see its own
    // `fetch_table_stats`.
    let size = match shape.total_size {
        true => "CASE WHEN c.relkind = 'v' THEN NULL ELSE pg_total_relation_size(c.oid) END::int8",
        false => "NULL::int8",
    };
    let sql = format!(
        "SELECT {reltuples}, {size} \
         FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace \
         WHERE n.nspname = COALESCE($1::text, current_schema()) AND c.relname::text = $2"
    );
    let row = sqlx::query_as::<_, (Option<f64>, Option<i64>)>(&sql)
        .bind(table.schema.as_deref())
        .bind(&table.name)
        .fetch_optional(pool)
        .await
        .map_err(catalog_error)?;
    // No row at all means the relation is gone (dropped under a stale schema
    // tree), which is an absence like any other rather than an error.
    let Some((reltuples, bytes)) = row else {
        return Ok(TableStats::default());
    };
    Ok(TableStats {
        rows: row_estimate(reltuples).map(RowCount::Estimated),
        bytes: size_bytes(bytes),
    })
}

/// Turns a raw `pg_class` row estimate into a number worth showing, or `None`.
///
/// **`reltuples < 0` is the only thing dropped, and a zero is kept.**
/// PostgreSQL 14 and later store `-1` for a relation that has never been
/// vacuumed or analyzed, precisely so it is distinguishable from an empty one —
/// which is exactly the distinction this needs, and every Postgres-wire engine
/// in the matrix spells it that way. Verified rather than assumed: a populated,
/// never-analyzed table reports `reltuples = -1` on PostgreSQL 17.10,
/// TimescaleDB 17.10, Citus 18.4 and YugabyteDB 15.12 alike, and CockroachDB
/// reports NULL. So the sentinel alone separates "nobody has counted this" from
/// "counted, and the answer was none".
///
/// A measured zero is therefore reported *as zero*. It is available, it is
/// true, and on a table someone has just opened in an unfamiliar database it is
/// very plausibly the most useful thing the pane can say — certainly better than
/// a blank that reads as "unknown" and invites an exact count to discover the
/// same thing. It is also what SQL Server's maintained counter reports for the
/// identical table, and the two backends must not disagree about it.
///
/// An earlier version also dropped `reltuples = 0` alongside `relpages = 0`,
/// reasoning that PostgreSQL 13 and older have no `-1` and spell "unmeasured"
/// that way. That clause never fired for the case it described on any server
/// here — the `-1` covers it — and only ever suppressed genuine zeroes, so it is
/// gone, and `relpages` with it. If a pre-14 server ever enters the support
/// matrix, the fix is not to reinstate the guess: `pg_stat_get_last_analyze_time`
/// answers "has this ever been analyzed" as a catalog fact, which is what
/// CLAUDE.md's rule about version-dependent behaviour asks for.
fn row_estimate(reltuples: Option<f64>) -> Option<u64> {
    let reltuples = reltuples?;
    if reltuples < 0.0 || !reltuples.is_finite() {
        return None;
    }
    Some(reltuples.round() as u64)
}

/// What a `pg_dump` of the same table carries and this rebuild does not,
/// regardless of how the catalog read went. Named rather than implied, so a
/// reader knows the boundary.
fn pg_standing_caveats() -> Vec<String> {
    vec![
        "sequences behind nextval() defaults".into(),
        "storage parameters, tablespace, partitioning and inheritance".into(),
        "triggers and row-level security policies".into(),
        "comments, ownership and privileges".into(),
    ]
}

/// Reads the per-table facts a faithful `CREATE TABLE` needs: exact column
/// types, defaults, identity/generated clauses, non-default collations, every
/// constraint as the server renders it, and the indexes that do not back one
/// of this table's own constraints (those are already covered by their
/// constraint).
async fn table_ddl_extras(pool: &PgPool, params: &[Value]) -> Result<TableExtras, DbError> {
    // A column's collation only matters when it differs from its type's
    // default; emitting the default one everywhere would be noise.
    let column_rows = query_with(
        pool,
        "SELECT a.attname::text AS name, \
                format_type(a.atttypid, a.atttypmod) AS type_name, \
                pg_get_expr(d.adbin, d.adrelid) AS default_expr, \
                a.attidentity::text AS identity, \
                a.attgenerated::text AS generated, \
                CASE WHEN a.attcollation <> t.typcollation THEN co.collname::text END AS collation \
         FROM pg_attribute a \
         JOIN pg_class c ON c.oid = a.attrelid \
         JOIN pg_namespace n ON n.oid = c.relnamespace \
         JOIN pg_type t ON t.oid = a.atttypid \
         LEFT JOIN pg_attrdef d ON d.adrelid = a.attrelid AND d.adnum = a.attnum \
         LEFT JOIN pg_collation co ON co.oid = a.attcollation \
         WHERE n.nspname::text = $1 AND c.relname::text = $2 \
           AND a.attnum > 0 AND NOT a.attisdropped \
         ORDER BY a.attnum",
        params,
    )
    .await?;

    // Not-null constraints are also in pg_constraint from PG 17 (contype
    // 'n'); they are already rendered per column, so only the table-level
    // types are taken.
    let constraint_rows = query_with(
        pool,
        "SELECT con.conname::text AS name, pg_get_constraintdef(con.oid) AS def \
         FROM pg_constraint con \
         JOIN pg_class c ON c.oid = con.conrelid \
         JOIN pg_namespace n ON n.oid = c.relnamespace \
         WHERE n.nspname::text = $1 AND c.relname::text = $2 \
           AND con.contype IN ('p', 'u', 'f', 'c', 'x') \
         ORDER BY CASE con.contype \
                    WHEN 'p' THEN 0 WHEN 'u' THEN 1 WHEN 'f' THEN 2 WHEN 'c' THEN 3 ELSE 4 \
                  END, con.conname",
        params,
    )
    .await?;

    // An index is skipped only when one of the three constraint kinds that
    // *own* an index is built on it. `conindid` is set on a foreign key too,
    // but there it names the index on the *referenced* table that the key
    // leans on — so asking "does any constraint name this index" dropped a
    // bare `CREATE UNIQUE INDEX` from its own table's DDL the moment another
    // table referenced it, and replaying that DDL could not recreate the
    // referencing table. A unique *constraint* on the referenced side is
    // unaffected: it is skipped here and rendered by its own `u` row above.
    let index_rows = query_with(
        pool,
        "SELECT pg_get_indexdef(i.indexrelid) AS def \
         FROM pg_index i \
         JOIN pg_class c ON c.oid = i.indrelid \
         JOIN pg_class ic ON ic.oid = i.indexrelid \
         JOIN pg_namespace n ON n.oid = c.relnamespace \
         WHERE n.nspname::text = $1 AND c.relname::text = $2 AND i.indisvalid \
           AND NOT EXISTS ( \
             SELECT 1 FROM pg_constraint con \
             WHERE con.conindid = i.indexrelid AND con.contype IN ('p', 'u', 'x') \
           ) \
         ORDER BY ic.relname",
        params,
    )
    .await?;

    let mut extras = TableExtras {
        caveats: pg_standing_caveats(),
        // The read succeeded, so an empty list here means "this table has no
        // table-level constraints", not "we could not tell".
        constraints: Some(Vec::new()),
        ..TableExtras::default()
    };
    for row in &column_rows.rows {
        let name = row_text(row, 0);
        let default_expr = row_opt_text(row, 2);
        // A generated column keeps its generation expression in pg_attrdef
        // too, so it must render as a generation clause and NOT as a DEFAULT.
        let (identity, generation_used) = match row_text(row, 3).as_str() {
            "a" => (Some("GENERATED ALWAYS AS IDENTITY".to_string()), false),
            "d" => (Some("GENERATED BY DEFAULT AS IDENTITY".to_string()), false),
            _ => match (row_text(row, 4).as_str(), &default_expr) {
                ("", _) => (None, false),
                (kind, Some(expr)) => {
                    // attgenerated: 's' = STORED, 'v' = VIRTUAL (PG 18, and
                    // the default for a bare GENERATED ALWAYS AS there).
                    // Guessing STORED for a virtual column would silently
                    // materialize it, so an unrecognized kind is written
                    // without a storage keyword and caveated instead.
                    let storage = match kind {
                        "s" => " STORED",
                        "v" => " VIRTUAL",
                        other => {
                            extras.caveats.push(format!(
                                "the storage kind of generated column {name} \
                                 (unrecognized pg_attribute.attgenerated {other:?})"
                            ));
                            ""
                        }
                    };
                    (Some(format!("GENERATED ALWAYS AS ({expr}){storage}")), true)
                }
                (_, None) => (None, false),
            },
        };
        extras.columns.insert(
            name,
            ColumnExtra {
                type_name: row_opt_text(row, 1),
                collation: row_opt_text(row, 5),
                // Postgres has no column form that replaces the type.
                computed: None,
                computed_persisted: false,
                identity,
                default: if generation_used { None } else { default_expr },
                // Postgres does not name defaults.
                default_constraint: None,
            },
        );
    }
    let constraints = extras.constraints.get_or_insert_with(Vec::new);
    for row in &constraint_rows.rows {
        constraints.push(format!(
            "CONSTRAINT {} {}",
            super::sql::quote_ident(&row_text(row, 0)),
            row_text(row, 1)
        ));
    }
    for row in &index_rows.rows {
        extras.indexes.push(row_text(row, 0));
    }
    Ok(extras)
}

/// Decodes scalar and rich Postgres types into the backend-neutral [`Value`]
/// model. Rich types (dates, numeric, uuid, json, intervals, arrays, enums)
/// render as `Value::Text` in a form close to what `psql` would print.
///
/// Cell data never errors the page: a type without a dedicated arm — or one
/// whose dedicated decode fails (NaN or >28-digit numeric, multidimensional
/// array, …) — degrades through [`decode_fallback`] to a text cast where
/// possible, then to a `<typename>` marker. The only `Err` left is an
/// out-of-range column index, which is a programming error.
fn decode_value(row: &PgRow, idx: usize) -> Result<Value, DbError> {
    let raw = row
        .try_get_raw(idx)
        .map_err(|e| DbError::Query(e.to_string()))?;
    if raw.is_null() {
        return Ok(Value::Null);
    }
    Ok(decode_typed(row, idx, &raw).unwrap_or_else(|| decode_fallback(row, idx, &raw)))
}

/// Type-specific decoding. Returns `None` both for types without a
/// dedicated arm and for values a dedicated arm cannot represent; the
/// caller degrades those via [`decode_fallback`].
fn decode_typed(row: &PgRow, idx: usize, raw: &PgValueRef) -> Option<Value> {
    match raw.type_info().name() {
        // Booleans render as text: "true"/"false" reads better in a viewer
        // than 0/1.
        "BOOL" => Some(Value::Text(row.try_get::<bool, _>(idx).ok()?.to_string())),
        "INT2" => Some(Value::Integer(row.try_get::<i16, _>(idx).ok()? as i64)),
        "INT4" => Some(Value::Integer(row.try_get::<i32, _>(idx).ok()? as i64)),
        "INT8" => Some(Value::Integer(row.try_get::<i64, _>(idx).ok()?)),
        "FLOAT4" => Some(Value::Real(real_from_f32(row.try_get::<f32, _>(idx).ok()?))),
        "FLOAT8" => Some(Value::Real(row.try_get::<f64, _>(idx).ok()?)),
        // `CHAR` is sqlx's name for `bpchar`, the type behind `char(n)` —
        // there is no `BPCHAR` to match. The single-byte catalog type
        // `"char"` (`pg_class.relkind` and most catalog flags) is a different
        // type that sqlx names with the quotes kept, and has its own arm.
        "TEXT" | "VARCHAR" | "CHAR" | "NAME" => {
            Some(Value::Text(row.try_get::<String, _>(idx).ok()?))
        }
        "\"CHAR\"" => {
            let bytes = raw.as_bytes().ok()?;
            match raw.format() {
                PgValueFormat::Text => Some(Value::Text(std::str::from_utf8(bytes).ok()?.into())),
                PgValueFormat::Binary => match bytes {
                    [byte] => Some(Value::Text(internal_char_text(*byte))),
                    _ => None,
                },
            }
        }
        "BYTEA" => Some(Value::Blob(row.try_get::<Vec<u8>, _>(idx).ok()?)),
        // Date/time and numeric, rendered from the wire bytes as the server
        // prints them — see `pg_wire` for what sqlx's chrono and
        // rust_decimal decodes got wrong, one of which was a panic.
        "TIMESTAMP" => wire_text(raw, |bytes| pg_wire::timestamp(bytes, "")),
        // The wire holds an instant; it is rendered in UTC with an explicit
        // offset so the timezone-awareness is visible.
        "TIMESTAMPTZ" => wire_text(raw, |bytes| pg_wire::timestamp(bytes, "+00:00")),
        "DATE" => wire_text(raw, pg_wire::date),
        "TIME" => wire_text(raw, pg_wire::time),
        "TIMETZ" => wire_text(raw, pg_wire::time_tz),
        "INTERVAL" => {
            let iv = row.try_get::<PgInterval, _>(idx).ok()?;
            Some(Value::Text(format_interval(&iv)))
        }
        // Exact, never through f64: every digit and the column's scale.
        "NUMERIC" => wire_text(raw, pg_wire::numeric),
        "UUID" => Some(Value::Text(row.try_get::<Uuid, _>(idx).ok()?.to_string())),
        // An unsigned 32-bit number: as an integer it is shown as the server
        // prints it and binds back through `::oid`. Without this arm it read
        // as the marker `<oid>`, and so did every `oid` key's locator.
        "OID" => Some(Value::Integer(i64::from(
            row.try_get::<Oid, _>(idx).ok()?.0,
        ))),
        // Bit strings, as the `0`/`1` text the server prints and the editor
        // accepts back. sqlx has no built-in decode for these
        // without the `bit-vec` feature, so without this arm they degraded to
        // the `<bit>` marker — which is not merely a display gap: that marker
        // becomes the row locator for a `bit(n)` primary key, so the save it
        // keys with is refused ("<" is not a valid binary digit) and such a
        // table is unsaveable however correct the cast is.
        "BIT" | "VARBIT" => wire_text(raw, bits_from_binary),
        // Compact JSON text (serde_json Display is compact).
        "JSON" | "JSONB" => Some(Value::Text(
            row.try_get::<JsonValue, _>(idx).ok()?.to_string(),
        )),
        // Arrays of the common element types render as a Postgres-style
        // literal. NULL elements render as NULL; text elements are not
        // quoted/escaped (this is a display form, not parseable syntax).
        // sqlx only decodes one-dimensional arrays; others degrade.
        "TEXT[]" | "VARCHAR[]" | "CHAR[]" | "NAME[]" => decode_array::<String>(row, idx, |v| v),
        "\"CHAR\"[]" => decode_array::<i8>(row, idx, |v| internal_char_text(v as u8)),
        "INT2[]" => decode_array::<i16>(row, idx, |v| v.to_string()),
        "INT4[]" => decode_array::<i32>(row, idx, |v| v.to_string()),
        "INT8[]" => decode_array::<i64>(row, idx, |v| v.to_string()),
        "FLOAT4[]" => decode_array::<f32>(row, idx, |v| v.to_string()),
        "FLOAT8[]" => decode_array::<f64>(row, idx, |v| v.to_string()),
        "BOOL[]" => decode_array::<bool>(row, idx, |v| v.to_string()),
        "UUID[]" => decode_array::<Uuid>(row, idx, |v| v.to_string()),
        "NUMERIC[]" => wire_array(raw, pg_wire::numeric),
        _ => None,
    }
}

/// One byte of Postgres's internal `"char"` type, as PostgreSQL 15 and later
/// print it: nothing for a zero byte, a backslash and three octal digits for
/// one with the high bit set (it is a byte, not a character, so the first byte
/// of a multi-byte UTF-8 character lands here), the character itself
/// otherwise. Those versions also read `\ooo` back as that byte, so the text
/// round-trips through an edit.
///
/// PostgreSQL 14 and earlier print a high byte raw and read only the first
/// byte of their input, so `\303` written back there stores a backslash
/// (measured on 14.24). Every PostgreSQL-based engine hubro is verified
/// against is 15 or later — the oldest, YugabyteDB, is 15.12 — and a
/// `"char"` with the high bit set is rare outside deliberately odd data.
fn internal_char_text(byte: u8) -> String {
    match byte {
        0 => String::new(),
        0x80.. => format!("\\{byte:03o}"),
        _ => char::from(byte).to_string(),
    }
}

/// A value whose text format is already the text to show, and whose binary
/// format `binary` renders: bit strings, and the [`pg_wire`] types.
///
/// The text half is present for completeness of the match rather than for
/// coverage: every row-returning path here goes through `sqlx::query`, which
/// carries arguments and therefore always negotiates the binary format.
fn wire_text(raw: &PgValueRef, binary: impl FnOnce(&[u8]) -> Option<String>) -> Option<Value> {
    let bytes = raw.as_bytes().ok()?;
    match raw.format() {
        PgValueFormat::Text => Some(Value::Text(std::str::from_utf8(bytes).ok()?.to_string())),
        PgValueFormat::Binary => binary(bytes).map(Value::Text),
    }
}

/// A one-dimensional array of a [`pg_wire`] type, as the same literal
/// [`decode_array`] builds: `{1.50,NaN,NULL}`. Binary format only — the
/// arrays the other arms decode are not read from text either.
fn wire_array(raw: &PgValueRef, element: impl Fn(&[u8]) -> Option<String>) -> Option<Value> {
    if raw.format() != PgValueFormat::Binary {
        return None;
    }
    let parts = pg_wire::array_elements(raw.as_bytes().ok()?)?
        .into_iter()
        .map(|item| item.map_or(Some("NULL".to_string()), &element))
        .collect::<Option<Vec<_>>>()?;
    Some(Value::Text(format!("{{{}}}", parts.join(","))))
}

/// The binary half of a bit string's [`wire_text`], split out so its rejections are
/// reachable from a test: `PgValueRef` cannot be constructed outside sqlx, so
/// left inline these would be four claims nothing could execute.
///
/// `None` for anything malformed — the caller then degrades to the `<bit>`
/// marker, which beats indexing past the end of the payload.
fn bits_from_binary(bytes: &[u8]) -> Option<String> {
    let (count, packed) = bytes.split_at_checked(4)?;
    let count = usize::try_from(i32::from_be_bytes(count.try_into().ok()?)).ok()?;
    // The bound is exact, not conservative: the last index read is
    // `(count - 1) / 8`, which is `<= packed.len() - 1` precisely when
    // `count <= packed.len() * 8`.
    if count > packed.len() * 8 {
        return None;
    }
    Some(
        (0..count)
            .map(|bit| {
                let byte = packed[bit / 8];
                if (byte >> (7 - bit % 8)) & 1 == 1 {
                    '1'
                } else {
                    '0'
                }
            })
            .collect(),
    )
}

/// Graceful degradation for values [`decode_typed`] can't produce: enum
/// labels from the raw bytes, then a text cast, then a `<typename>` marker.
/// Infallible by design — one odd cell must not take down the page.
fn decode_fallback(row: &PgRow, idx: usize, raw: &PgValueRef) -> Value {
    // User-defined enums: the wire value (text or binary format) is the
    // label itself, but `try_get::<String>` refuses the unknown OID, so
    // read the raw bytes directly.
    if matches!(raw.type_info().kind(), PgTypeKind::Enum(_)) {
        if let Ok(label) = raw.as_str() {
            return Value::Text(label.to_string());
        }
    } else if let Ok(text) = row.try_get::<String, _>(idx) {
        return Value::Text(text);
    }
    Value::Text(format!("<{}>", raw.type_info().name().to_lowercase()))
}

/// Decodes a one-dimensional array column and renders it as a
/// Postgres-style literal, e.g. `{a,b,NULL}`.
fn decode_array<T>(row: &PgRow, idx: usize, fmt: impl Fn(T) -> String) -> Option<Value>
where
    T: for<'a> sqlx::Decode<'a, sqlx::Postgres> + sqlx::Type<sqlx::Postgres> + PgHasArrayType,
{
    let items: Vec<Option<T>> = row.try_get(idx).ok()?;
    let mut parts = Vec::with_capacity(items.len());
    for item in items {
        parts.push(match item {
            Some(v) => fmt(v),
            None => "NULL".to_string(),
        });
    }
    Some(Value::Text(format!("{{{}}}", parts.join(","))))
}

/// Compact human form for an interval, e.g. "1 mon 2 days 03:04:05.5".
/// Zero parts are omitted; a zero interval renders as "00:00:00". Each
/// part carries its own sign, matching how Postgres prints mixed-sign
/// intervals.
fn format_interval(iv: &PgInterval) -> String {
    let mut parts: Vec<String> = Vec::new();
    if iv.months != 0 {
        let unit = if iv.months.abs() == 1 { "mon" } else { "mons" };
        parts.push(format!("{} {unit}", iv.months));
    }
    if iv.days != 0 {
        let unit = if iv.days.abs() == 1 { "day" } else { "days" };
        parts.push(format!("{} {unit}", iv.days));
    }
    if iv.microseconds != 0 || parts.is_empty() {
        let sign = if iv.microseconds < 0 { "-" } else { "" };
        let abs = iv.microseconds.unsigned_abs();
        let secs = abs / 1_000_000;
        let micros = abs % 1_000_000;
        let (h, m, s) = (secs / 3600, secs / 60 % 60, secs % 60);
        let mut time = format!("{sign}{h:02}:{m:02}:{s:02}");
        if micros != 0 {
            time.push_str(format!(".{micros:06}").trim_end_matches('0'));
        }
        parts.push(time);
    }
    parts.join(" ")
}

#[cfg(test)]
mod tests {
    /// How the rest of a read reports, from the ping that read it after the
    /// run: the server's error ended the statement (and undid it); any other
    /// error leaves the outcome unknown.
    #[test]
    fn a_rest_read_after_the_run_ends_as_its_read_says() {
        use super::late_rest;
        use crate::db::LateRest;
        #[derive(Debug)]
        struct ServerSaid;
        impl std::fmt::Display for ServerSaid {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("division by zero")
            }
        }
        impl std::error::Error for ServerSaid {}
        impl sqlx::error::DatabaseError for ServerSaid {
            fn message(&self) -> &str {
                "division by zero"
            }
            fn as_error(&self) -> &(dyn std::error::Error + Send + Sync + 'static) {
                self
            }
            fn as_error_mut(&mut self) -> &mut (dyn std::error::Error + Send + Sync + 'static) {
                self
            }
            fn into_error(self: Box<Self>) -> Box<dyn std::error::Error + Send + Sync + 'static> {
                self
            }
            fn kind(&self) -> sqlx::error::ErrorKind {
                sqlx::error::ErrorKind::Other
            }
        }
        assert_eq!(late_rest(Ok(())), LateRest::Finished);
        assert_eq!(
            late_rest(Err(sqlx::Error::Database(Box::new(ServerSaid)))),
            LateRest::Failed("division by zero".into())
        );
        for lost in [
            sqlx::Error::Io(std::io::Error::other("connection reset")),
            sqlx::Error::PoolClosed,
            sqlx::Error::Protocol("unexpected message".into()),
        ] {
            assert_eq!(late_rest(Err(lost)), LateRest::Unknown);
        }
    }

    /// A background read dropped before it reported is unknown, not pending
    /// for ever.
    #[tokio::test]
    async fn a_rest_whose_read_is_dropped_unreported_is_unknown() {
        use crate::db::{LateRest, PendingRest};
        let (sender, receiver) = tokio::sync::oneshot::channel::<LateRest>();
        drop(sender);
        assert_eq!(PendingRest(receiver).outcome().await, LateRest::Unknown);
    }

    /// Which catalog the column query asks for keys, for every flavor, and
    /// what each form of the query reads. The engine tests that measure the
    /// reason (`tests/db_risingwave.rs`) run only where RisingWave does; this
    /// runs everywhere, so sending RisingWave back to the views fails CI.
    #[test]
    fn only_risingwave_reads_its_keys_from_pg_constraint() {
        use super::{column_query, pk_source, ColumnQuery, PgFlavor, PkSource};
        // Each flavor's place in declaration order and the source it must
        // read, by an exhaustive match: a new flavor fails to compile here
        // until it states both.
        let placed = |flavor: PgFlavor| match flavor {
            PgFlavor::Postgres => (0, PkSource::InformationSchema),
            PgFlavor::CockroachDB => (1, PkSource::InformationSchema),
            PgFlavor::Yugabyte => (2, PkSource::InformationSchema),
            PgFlavor::Materialize => (3, PkSource::InformationSchema),
            PgFlavor::RisingWave => (4, PkSource::PgConstraint),
        };
        let expected = |flavor: PgFlavor| placed(flavor).1;
        let flavors = [
            PgFlavor::Postgres,
            PgFlavor::CockroachDB,
            PgFlavor::Yugabyte,
            PgFlavor::Materialize,
            PgFlavor::RisingWave,
        ];
        // The loop below covers what the match names: each flavor listed
        // once, in its place, and in declaration order, so a flavor declared
        // between two listed ones cannot be left out of it. A flavor declared
        // last and given the next place could still go unlisted; plain Rust
        // cannot enumerate an enum's variants.
        for (index, flavor) in flavors.iter().enumerate() {
            assert_eq!(placed(*flavor).0, index, "{flavor:?} out of place");
            assert_eq!(
                *flavor as usize, index,
                "{flavor:?} not in declaration order"
            );
        }
        for flavor in flavors {
            assert_eq!(pk_source(flavor), expected(flavor), "{flavor:?}");
            // The whole path, flavor to SQL: what the query sent for this
            // flavor actually reads.
            for shape in [ColumnQuery::Rich, ColumnQuery::Portable] {
                let sql = column_query(shape, flavor);
                match expected(flavor) {
                    PkSource::PgConstraint => {
                        assert!(
                            sql.contains("FROM pg_constraint con")
                                && sql.contains(
                                    "unnest(con.conkey) WITH ORDINALITY AS k(attnum, ord)"
                                )
                                && sql.contains("k.ord AS ordinal_position"),
                            "{flavor:?} {shape:?}: {sql}"
                        );
                        assert!(
                            !sql.contains("table_constraints") && !sql.contains("key_column_usage"),
                            "{flavor:?} {shape:?}: still reads a view that raises: {sql}"
                        );
                    }
                    PkSource::InformationSchema => assert!(
                        sql.contains("information_schema.table_constraints")
                            && sql.contains("information_schema.key_column_usage")
                            && !sql.contains("conkey"),
                        "{flavor:?} {shape:?}: {sql}"
                    ),
                }
            }
        }
        // And introspection hands the query the connection's own flavor: the
        // one call, as written (whitespace collapsed, comments and tests out).
        let shipped: String = crate::source_scan::code_only(&crate::source_scan::shipped(
            include_str!("postgres.rs"),
        ));
        let calls: Vec<&str> = shipped
            .match_indices("column_query(")
            .map(|(at, _)| &shipped[at..at + shipped[at..].find(')').unwrap() + 1])
            .filter(|call| !call.contains(": ColumnQuery"))
            .collect();
        assert_eq!(
            calls,
            ["column_query(shape.column_query, conn.flavor()"],
            "introspection asks the column query for another flavor than its own"
        );
    }

    /// The extension query's rows as markings: an extension beats a
    /// partition on the same object whichever row comes first, a schema-wide
    /// row marks the schema, and which extensions are installed is read from
    /// those rows alone.
    #[test]
    fn extension_rows_mark_schemas_and_objects_and_say_what_is_installed() {
        use super::{fold_internal_rows, Installed, Internal};
        let row = |schema: &str, object: Option<&str>, extension: Option<&str>| {
            (
                schema.to_string(),
                object.map(str::to_string),
                extension.map(str::to_string),
            )
        };
        let key = ("public".to_string(), "chunk".to_string());
        for rows in [
            vec![
                row("public", Some("chunk"), None),
                row("public", Some("chunk"), Some("timescaledb")),
            ],
            vec![
                row("public", Some("chunk"), Some("timescaledb")),
                row("public", Some("chunk"), None),
            ],
        ] {
            let (marks, installed) = fold_internal_rows(rows);
            assert_eq!(
                marks.objects[&key],
                Internal::Extension("timescaledb".into())
            );
            assert_eq!(
                installed,
                Installed {
                    citus: false,
                    timescale: true
                }
            );
        }
        let (marks, installed) = fold_internal_rows(vec![
            row("citus", None, Some("citus")),
            row("sales", Some("orders_1"), None),
        ]);
        assert_eq!(marks.schemas["citus"], Internal::Extension("citus".into()));
        assert_eq!(
            marks.objects[&("sales".to_string(), "orders_1".to_string())],
            Internal::Partition
        );
        assert!(installed.citus && !installed.timescale);
        let (_, installed) = fold_internal_rows(vec![row("p", Some("part"), None)]);
        assert_eq!(installed, Installed::default());
    }

    /// One listed relation: its kind from the server's `table_type`, its
    /// internal marking most specific first (object, then the engine's own
    /// classification, then the schema), its label, and its restriction.
    #[test]
    fn a_relation_takes_its_kind_marking_label_and_restriction_from_the_phases() {
        use super::{
            relation_meta, InternalMarks, Restriction, TableKind, INSTEAD_RULE, STREAMING_SINK,
            STREAMING_SOURCE,
        };
        use std::collections::{HashMap, HashSet};
        let mut marks = InternalMarks::default();
        marks
            .schemas
            .insert("ext".into(), Internal::Extension("postgis".into()));
        marks
            .objects
            .insert(("ext".into(), "own".into()), Internal::Partition);
        let mut labels = HashMap::new();
        labels.insert(("s".into(), "hyper".into()), "hypertable".to_string());
        labels.insert(("s".into(), "src".into()), "labelled".to_string());
        let mut rules = HashSet::new();
        rules.insert(("s".to_string(), "ruled".to_string()));
        let meta = |schema: &str, name: &str, table_type: &str| {
            relation_meta(
                schema.into(),
                name.into(),
                table_type,
                &marks,
                &labels,
                &rules,
            )
        };
        use super::Internal;
        assert_eq!(
            meta("ext", "own", "SYSTEM VIEW").internal,
            Some(Internal::Partition)
        );
        assert_eq!(
            meta("s", "sys", "SYSTEM TABLE").internal,
            Some(Internal::System)
        );
        assert_eq!(
            meta("ext", "other", "SYSTEM VIEW").internal,
            Some(Internal::System)
        );
        assert_eq!(
            meta("ext", "other", "BASE TABLE").internal,
            Some(Internal::Extension("postgis".into()))
        );
        assert_eq!(meta("s", "t", "BASE TABLE").internal, None);
        for (table_type, kind) in [
            ("BASE TABLE", TableKind::Table),
            ("VIEW", TableKind::View),
            ("SYSTEM VIEW", TableKind::View),
            ("SOURCE", TableKind::View),
            ("SINK", TableKind::View),
            ("MATERIALIZED VIEW", TableKind::MaterializedView),
        ] {
            assert_eq!(meta("s", "t", table_type).kind, kind, "{table_type}");
        }
        assert_eq!(
            meta("s", "hyper", "BASE TABLE").kind_label.as_deref(),
            Some("hypertable")
        );
        assert_eq!(
            meta("s", "src", "SOURCE").kind_label.as_deref(),
            Some("labelled")
        );
        assert_eq!(
            meta("s", "t", "SOURCE").kind_label.as_deref(),
            Some("source")
        );
        assert_eq!(meta("s", "t", "SINK").kind_label.as_deref(), Some("sink"));
        assert_eq!(
            meta("s", "t", "SOURCE").restriction,
            Some(Restriction::Declared(STREAMING_SOURCE))
        );
        assert_eq!(
            meta("s", "t", "SINK").restriction,
            Some(Restriction::NoRows(STREAMING_SINK))
        );
        assert_eq!(
            meta("s", "ruled", "BASE TABLE").restriction,
            Some(INSTEAD_RULE)
        );
        assert_eq!(meta("s", "t", "BASE TABLE").restriction, None);
    }

    /// A marked browse whose `BEGIN` opens the transaction and then fails —
    /// CockroachDB's two-statement one, standing in for a `SET LOCAL` the
    /// server refuses — leaves no aborted transaction on a pooled
    /// connection. Through a pool of one, the next query gets either a new
    /// connection or a clean one; returned as it was, it failed with
    /// "current transaction is aborted".
    #[tokio::test]
    async fn a_browse_whose_begin_fails_does_not_strand_its_connection() {
        let Ok(url) = std::env::var("HUBRO_PG_TEST_URL") else {
            eprintln!("skipping: HUBRO_PG_TEST_URL not set");
            return;
        };
        const FAILING_BEGIN: &str = "BEGIN READ ONLY; SET LOCAL hubro_no_such_setting = off";
        let pg = super::open_postgres_sized(&url, 1).await.unwrap();
        let pool = pg.pool();
        let (read, _) = super::read_only_query_on(pool, FAILING_BEGIN, "SELECT 1", &[]).await;
        assert!(read.is_err(), "the failing BEGIN failed nothing: {read:?}");
        super::query_with(pool, "SELECT 1", &[])
            .await
            .expect("the read after a failed browse BEGIN");
        let mut out = Vec::new();
        let (export, _) = super::read_only_export_on(
            pool,
            FAILING_BEGIN,
            "SELECT 1",
            &[],
            super::ExportFormat::Csv,
            &mut out,
        )
        .await;
        assert!(
            export.is_err(),
            "the failing BEGIN failed nothing: {export:?}"
        );
        super::query_with(pool, "SELECT 1", &[])
            .await
            .expect("the read after a failed export BEGIN");
        pool.close().await;
    }

    use super::*;

    #[test]
    fn only_postgres_is_trusted_to_say_whether_a_statement_has_written() {
        assert!(knows_whether_written(PgFlavor::Postgres));
        // Its `backend_xid` stayed empty while a function inserted rows.
        assert!(!knows_whether_written(PgFlavor::Yugabyte));
        // No `pg_stat_activity` row for the session.
        assert!(!knows_whether_written(PgFlavor::CockroachDB));
        assert!(!knows_whether_written(PgFlavor::Materialize));
        assert!(!knows_whether_written(PgFlavor::RisingWave));
    }

    #[test]
    fn a_cut_short_read_is_cancelled_only_where_a_cancel_was_measured_to_stop_it() {
        assert_eq!(cancel_kind(PgFlavor::Postgres), Some(CancelKind::Backend));
        assert_eq!(cancel_kind(PgFlavor::Yugabyte), Some(CancelKind::Backend));
        // No `pg_cancel_backend` at all.
        assert_eq!(
            cancel_kind(PgFlavor::CockroachDB),
            Some(CancelKind::CockroachSession)
        );
        // Answers true and goes on sending; and a backend id of 0.
        assert_eq!(cancel_kind(PgFlavor::Materialize), None);
        assert_eq!(cancel_kind(PgFlavor::RisingWave), None);
    }

    /// A refused probe counts as a rollback — the `ROLLBACK` after it discards
    /// whatever was open — except CockroachDB's commit-wait block, which
    /// follows a commit. The same `25000` from Materialize is uncommitted work.
    #[test]
    fn a_refused_probe_is_a_rollback_unless_cockroach_already_committed() {
        use PgFlavor::*;
        assert!(!refused_probe_rolled_back(CockroachDB, Some("25000")));
        assert!(refused_probe_rolled_back(CockroachDB, Some("25P02")));
        assert!(refused_probe_rolled_back(Materialize, Some("25000")));
        assert!(refused_probe_rolled_back(Postgres, Some("25000")));
        assert!(refused_probe_rolled_back(Postgres, Some("25P02")));
        assert!(refused_probe_rolled_back(Postgres, None));
    }

    /// A bit-string wire value: big-endian `i32` count, then packed bytes.
    fn wire(count: i32, packed: &[u8]) -> Vec<u8> {
        let mut bytes = count.to_be_bytes().to_vec();
        bytes.extend_from_slice(packed);
        bytes
    }

    #[test]
    fn bit_strings_decode_to_their_bits_and_stop_at_the_count() {
        // The count, not the byte length, is what ends the value: the last
        // byte is padded, and reading it whole would append phantom bits.
        // `bit(7)` is the case that catches it in both directions.
        assert_eq!(
            bits_from_binary(&wire(7, &[0b1111_1110])).as_deref(),
            Some("1111111")
        );
        assert_eq!(
            bits_from_binary(&wire(7, &[0b0000_0010])).as_deref(),
            Some("0000001")
        );
        // MSB-first, across a byte boundary.
        assert_eq!(
            bits_from_binary(&wire(9, &[0b1000_0000, 0b1000_0000])).as_deref(),
            Some("100000001")
        );
        assert_eq!(
            bits_from_binary(&wire(4, &[0b1010_0000])).as_deref(),
            Some("1010")
        );
        // A zero-length `bit varying` is a legal value, and must not index.
        assert_eq!(bits_from_binary(&wire(0, &[])).as_deref(), Some(""));
    }

    #[test]
    fn a_malformed_bit_string_degrades_instead_of_panicking() {
        // Each of these would otherwise index past the payload. They are
        // reachable only because the binary half is its own function — the
        // reason it is one.
        assert_eq!(bits_from_binary(&[0, 0, 1]), None, "header truncated");
        assert_eq!(
            bits_from_binary(&wire(-1, &[0xFF])),
            None,
            "negative bit count"
        );
        assert_eq!(
            bits_from_binary(&wire(9, &[0xFF])),
            None,
            "count claims more bits than were sent"
        );
        assert_eq!(
            bits_from_binary(&wire(1, &[])),
            None,
            "count without payload"
        );
        // The bound is exact: one bit fewer than the claim is fine.
        assert_eq!(
            bits_from_binary(&wire(8, &[0xFF])).as_deref(),
            Some("11111111")
        );
    }

    #[test]
    fn an_internal_char_prints_as_the_server_prints_it() {
        assert_eq!(internal_char_text(b'r'), "r");
        assert_eq!(internal_char_text(b' '), " ");
        assert_eq!(internal_char_text(0x7f), "\u{7f}");
        // A byte, not a character: the high half is octal-escaped, which
        // is what `psql` shows for the same value.
        assert_eq!(internal_char_text(0x80), "\\200");
        assert_eq!(internal_char_text(0xc3), "\\303");
        assert_eq!(internal_char_text(0xff), "\\377");
        assert_eq!(internal_char_text(0), "");
    }

    /// A best-effort catalog row that does not decode costs that row and
    /// nothing else. Run on SQLite rows because a `PgRow` cannot be built
    /// without a server; the helper is generic for exactly this.
    ///
    /// The bad row is a retyped column rather than a NULL, because SQLite's
    /// driver decodes a NULL into a `String` as empty text instead of failing.
    /// On Postgres a NULL fails the same `try_get` as a type mismatch does
    /// (`UnexpectedNullError`), so it takes the same path.
    #[tokio::test]
    async fn a_best_effort_row_that_does_not_decode_is_skipped_alone() {
        use sqlx::Connection as _;
        let mut conn = sqlx::SqliteConnection::connect("sqlite::memory:")
            .await
            .unwrap();
        let rows = sqlx::query(
            "SELECT 'a' AS s, 'b' AS n \
             UNION ALL SELECT 'integer name', 7 \
             UNION ALL SELECT 'e', 'f'",
        )
        .fetch_all(&mut conn)
        .await
        .unwrap();
        assert_eq!(rows.len(), 3);
        // The bad row is in the middle, so stopping at it would lose the
        // last row as well.
        assert_eq!(
            best_effort_names(&rows, ["s", "n"]),
            [["a", "b"], ["e", "f"]].map(|row| row.map(String::from))
        );
        // Every column of a row is read: the schema alone decodes on all three.
        assert_eq!(best_effort_names(&rows, ["s"]).len(), 3);
        // A column the query does not have is a decode failure like any
        // other — every row goes, and still nothing is returned as an error.
        assert!(best_effort_names(&rows, ["s", "missing"]).is_empty());
    }

    #[test]
    fn an_unmeasured_relation_estimates_nothing() {
        // PostgreSQL 14+ says -1 for "never vacuumed or analyzed"; every table
        // in a freshly created or freshly restored database looks like this,
        // on all four Postgres-family engines in the matrix.
        assert_eq!(row_estimate(Some(-1.0)), None);
        // A column this server does not have (the shape probe selected NULL),
        // or one it leaves NULL, as CockroachDB does.
        assert_eq!(row_estimate(None), None);
        // Nothing sane produces these, and NaN would round into a garbage u64.
        assert_eq!(row_estimate(Some(f64::NAN)), None);
        assert_eq!(row_estimate(Some(f64::INFINITY)), None);
    }

    #[test]
    fn a_measured_zero_is_reported_as_zero() {
        // The regression this pins: an analyzed, genuinely empty table reports
        // `reltuples = 0`, and that zero is a measurement. Suppressing it left
        // the pane indistinguishable from "statistics unknown" while SQL
        // Server reported "0 rows" for the identical table.
        assert_eq!(row_estimate(Some(0.0)), Some(0));
    }

    #[test]
    fn a_measured_estimate_survives_and_rounds() {
        assert_eq!(row_estimate(Some(42.0)), Some(42));
        // reltuples is a float and routinely fractional after a sampled
        // ANALYZE of a partially filled page.
        assert_eq!(row_estimate(Some(9.6)), Some(10));
    }

    #[test]
    fn each_engine_is_identified_from_the_version_string_it_really_sends() {
        // Verbatim `version()` output from the containers the engine tests run
        // against, so this stays a test of real strings rather than of strings
        // written to match the parser.
        for (version, expected) in [
            (
                "PostgreSQL 17.10 on x86_64-pc-linux-musl, compiled by gcc \
                 (Alpine 15.2.0) 15.2.0, 64-bit",
                PgFlavor::Postgres,
            ),
            (
                "CockroachDB CCL v26.2.5 (x86_64-pc-linux-gnu, built 2026/07/28 18:56:00, \
                 go1.25.5)",
                PgFlavor::CockroachDB,
            ),
            (
                "PostgreSQL 15.12-YB-2026.1.0.1-b0 on x86_64-pc-linux-gnu, compiled by \
                 clang version 21.1.1 (https://github.com/yugabyte/llvm-project.git \
                 efca861cc42178cc4c555d605b36c79d7d121cc1), 64-bit",
                PgFlavor::Yugabyte,
            ),
            (
                "PostgreSQL 9.5 on x86_64-unknown-linux-gnu (Materialize 26.36.0)",
                PgFlavor::Materialize,
            ),
            (
                "PostgreSQL 13.14.0-RisingWave-3.0.2 \
                 (391c3a16ef26d0cd86d1236c9b7c122a9a27fb1e)",
                PgFlavor::RisingWave,
            ),
        ] {
            assert_eq!(detect_flavor(version), expected, "{version}");
        }
    }

    #[test]
    fn a_claimed_postgres_version_never_outvotes_the_engines_own_name() {
        // The trap this parser exists to avoid: Yugabyte and Materialize both
        // *lead* with "PostgreSQL <n>", so matching that first would file both
        // as stock and skip everything they need.
        for version in [
            "PostgreSQL 15.12-YB-2026.1.0.1-b0 on x86_64-pc-linux-gnu",
            "PostgreSQL 9.5 on x86_64-unknown-linux-gnu (Materialize 26.36.0)",
            "PostgreSQL 13.14.0-RisingWave-3.0.2 (391c3a16)",
        ] {
            assert_ne!(detect_flavor(version), PgFlavor::Postgres, "{version}");
        }
    }

    #[test]
    fn an_unrecognized_server_is_treated_as_plain_postgres() {
        // The safe default: a server nobody has taught this about gets the
        // standard catalog path and no special cases, which is exactly how it
        // was handled before it had a name at all.
        assert_eq!(detect_flavor(""), PgFlavor::Postgres);
        assert_eq!(
            detect_flavor("SomeFuturePostgresFork 1.0 on x86_64"),
            PgFlavor::Postgres
        );
    }

    #[test]
    fn detection_ignores_case() {
        assert_eq!(detect_flavor("cockroachdb ccl v26"), PgFlavor::CockroachDB);
        assert_eq!(detect_flavor("COCKROACHDB CCL V26"), PgFlavor::CockroachDB);
    }

    #[test]
    fn url_wrappers_bind_the_postgres_scheme() {
        // One probe per wrapper; the shared behavior is covered in db::url.
        assert_eq!(
            url_with_password("postgres://user@db.example.com:5432/app", "p%rd").unwrap(),
            "postgres://user:p%25rd@db.example.com:5432/app"
        );
        assert_eq!(
            normalize_pg_url("postgresql://user@Db.Example.COM/app").unwrap(),
            "postgres://user@db.example.com:5432/app"
        );
        assert_eq!(
            url_target("postgres://u@db.internal/app").unwrap(),
            ("db.internal".to_string(), 5432)
        );
        assert_eq!(
            url_via_local_port("postgres://u@db.internal:5432/app?sslmode=disable", 40123).unwrap(),
            "postgres://u@127.0.0.1:40123/app?sslmode=disable"
        );
        assert_eq!(
            build_url("db.example.com", "", "app", "user", "prefer").unwrap(),
            "postgres://user@db.example.com:5432/app?sslmode=prefer"
        );
    }

    #[test]
    fn connect_errors_are_categorized() {
        let auth = sqlx::Error::Protocol("password authentication failed for user \"x\"".into());
        assert!(friendly_connect_error(&auth).starts_with("authentication failed"));
        let net = sqlx::Error::Protocol("Connection refused (os error 111)".into());
        assert!(friendly_connect_error(&net).starts_with("network error"));
        let tls = sqlx::Error::Protocol("TLS handshake failed".into());
        assert!(friendly_connect_error(&tls).starts_with("TLS error"));
        // A typo'd username is not a password problem, and must not read like
        // one. (Whether it raises the password prompt is decided on the
        // SQLSTATE, by `connect_error_from`, not on these words.)
        let role = sqlx::Error::Protocol("role \"nope\" does not exist".into());
        let friendly = friendly_connect_error(&role);
        assert!(friendly.starts_with("unknown role"));
        assert!(!friendly.contains("authentication failed"));
        // A non-Postgres server answering the handshake (e.g. SQL Server on
        // 1433) mentions SSL but is a wrong-server problem, not a TLS one.
        let not_pg = sqlx::Error::Protocol(
            "encountered unexpected or invalid data: unexpected response from SSLRequest: 0x00"
                .into(),
        );
        let friendly = friendly_connect_error(&not_pg);
        assert!(friendly.starts_with("the server doesn't appear to be Postgres"));
        assert!(!friendly.starts_with("TLS error"));
        // An X.509 v1 server certificate. rustls's own wording names
        // neither "tls" nor "ssl", so without its own arm this would fall
        // through to the uncategorized branch and reach the user as
        // `UnsupportedCertVersion` — true, and useless.
        let v1 = sqlx::Error::Protocol(
            "error communicating with database: invalid peer certificate: \
             Other(OtherError(UnsupportedCertVersion))"
                .into(),
        );
        let friendly = friendly_connect_error(&v1);
        assert!(friendly.contains("X.509 v1"), "{friendly}");
        assert!(friendly.contains("sslmode=disable"), "{friendly}");
    }

    #[test]
    fn only_invalid_password_rejects_the_credentials() {
        // The connect flow prompts for a new password on CredentialsRejected,
        // and an unticked different answer deletes the stored one, so the
        // variant is a promise that the *password* was refused. The
        // SQLSTATEs below were measured against PostgreSQL 17, except where
        // a row says otherwise.
        let rejected = |code| connect_error_from(code, "m".into()).credentials_rejected();
        assert!(
            rejected(Some(INVALID_PASSWORD)),
            "a wrong password must raise the prompt"
        );
        for (code, what) in [
            (Some("3D000"), "a database that does not exist"),
            (Some("42501"), "a database the role may not CONNECT to"),
            (
                Some("28000"),
                "a pg_hba.conf reject (and, unmeasured, LDAP/PAM/ident)",
            ),
            (None, "a failure with no SQLSTATE at all"),
        ] {
            assert!(
                !rejected(code),
                "{what} ({code:?}) would delete a working password"
            );
        }
        // The text never decides: a password-failed message with no SQLSTATE
        // behind it is reported, and keeps the password…
        let text_only =
            sqlx::Error::Protocol("password authentication failed for user \"x\"".into());
        assert_eq!(
            connect_error(&text_only),
            DbError::Connect(friendly_connect_error(&text_only))
        );
        // …and the message survives the classification unchanged.
        assert_eq!(
            connect_error_from(Some(INVALID_PASSWORD), "the words".into()),
            DbError::CredentialsRejected("the words".into())
        );
    }

    #[test]
    fn the_pin_probe_asks_for_values_the_server_would_not_report_by_itself() {
        // Neither the pinned `postgres` nor the server default, so a pooler
        // dropping the option shows; `lc_monetary` only where the server has
        // it, and never the value it already has.
        assert_eq!(pin_probe(None), [("IntervalStyle", "iso_8601".to_string())]);
        assert_eq!(
            pin_probe(Some("en_US.utf8")),
            [
                ("IntervalStyle", "iso_8601".to_string()),
                ("lc_monetary", "C".to_string()),
            ]
        );
        assert_eq!(pin_probe(Some("C"))[1].1, "POSIX");
    }

    #[test]
    fn only_a_pooler_refusing_the_startup_options_is_retried_without_them() {
        // PgBouncer 1.18, then 1.21/1.26, measured.
        assert!(startup_options_refusal(
            Some("08P01"),
            "unsupported startup parameter: options"
        ));
        assert!(startup_options_refusal(
            Some("08P01"),
            "unsupported startup parameter in options: lc_monetary"
        ));
        // The same code for a failed SASL login (measured through PgBouncer),
        // a wrong password, another parameter, or no server answer at all:
        // none of them is the pins' fault.
        for (code, message) in [
            (Some("08P01"), "SASL authentication failed"),
            (Some("28P01"), "unsupported startup parameter: options"),
            (
                Some("08P01"),
                "unsupported startup parameter: application_name",
            ),
            (None, "unsupported startup parameter: options"),
        ] {
            assert!(
                !startup_options_refusal(code, message),
                "{code:?} {message}"
            );
        }
    }

    #[test]
    fn every_session_is_pinned_to_the_settings_key_text_depends_on() {
        // IntervalStyle always; lc_monetary to the value the first session
        // reported, escaped for the whitespace-split `options` string; and
        // nothing for it where the engine has no such setting.
        assert_eq!(
            key_text_pins(Some("en_US.utf8")),
            [
                ("IntervalStyle", "postgres".to_string()),
                ("lc_monetary", "en_US.utf8".to_string()),
            ]
        );
        assert_eq!(
            key_text_pins(None),
            [("IntervalStyle", "postgres".to_string())]
        );
        assert_eq!(
            key_text_pins(Some("Norwegian Bokmal\\x")),
            [
                ("IntervalStyle", "postgres".to_string()),
                ("lc_monetary", "Norwegian\\ Bokmal\\\\x".to_string()),
            ]
        );
    }

    #[test]
    fn interval_formats_compactly() {
        let iv = |months, days, microseconds| PgInterval {
            months,
            days,
            microseconds,
        };
        // All parts present, plural units, fractional seconds trimmed.
        assert_eq!(
            format_interval(&iv(14, 2, (3 * 3600 + 4 * 60 + 5) * 1_000_000)),
            "14 mons 2 days 03:04:05"
        );
        assert_eq!(
            format_interval(&iv(1, 1, 5_000_000 + 500_000)),
            "1 mon 1 day 00:00:05.5"
        );
        // Zero parts are omitted; all-zero renders a zero time.
        assert_eq!(format_interval(&iv(0, 3, 0)), "3 days");
        assert_eq!(format_interval(&iv(0, 0, 0)), "00:00:00");
        // Negative components each carry their sign.
        assert_eq!(
            format_interval(&iv(-1, -2, -5_000_000)),
            "-1 mon -2 days -00:00:05"
        );
    }

    #[test]
    fn line_col_maps_character_positions() {
        let sql = "SELECT 1 +\n  bad_col\nFROM t";
        assert_eq!(line_col(sql, 1), Some((1, 1)));
        assert_eq!(line_col(sql, 8), Some((1, 8)));
        // First char of line 2 ("SELECT 1 +\n" is 11 chars).
        assert_eq!(line_col(sql, 12), Some((2, 1)));
        assert_eq!(line_col(sql, 14), Some((2, 3)));
        // One past the end is valid (server points at missing input).
        let len = sql.chars().count();
        assert_eq!(line_col(sql, len + 1), Some((3, 7)));
        // Zero and far-out-of-range positions are dropped.
        assert_eq!(line_col(sql, 0), None);
        assert_eq!(line_col(sql, len + 2), None);
    }

    #[test]
    fn line_col_counts_characters_not_bytes() {
        // "ыыы" is 6 bytes but 3 chars; position 5 is the 'X'.
        assert_eq!(line_col("ыыыаX", 5), Some((1, 5)));
    }

    #[test]
    fn only_the_constraints_that_own_an_index_are_reported() {
        for flavor in [PgFlavor::Postgres, PgFlavor::Yugabyte] {
            assert_eq!(
                pg_index_constraint(Some("p"), flavor),
                Some(IndexConstraint::PrimaryKey)
            );
            assert_eq!(
                pg_index_constraint(Some("u"), flavor),
                Some(IndexConstraint::Unique)
            );
            assert_eq!(
                pg_index_constraint(Some("x"), flavor),
                Some(IndexConstraint::Exclusion)
            );
            // A foreign key's `conindid` names another table's index; the
            // query never selects one, and the mapping would not claim it.
            assert_eq!(pg_index_constraint(Some("f"), flavor), None);
            assert_eq!(pg_index_constraint(None, flavor), None);
        }
        // CockroachDB reports a user's unique index as a `u` constraint too,
        // so its `u` claims nothing; its primary key still does.
        assert_eq!(pg_index_constraint(Some("u"), PgFlavor::CockroachDB), None);
        assert_eq!(
            pg_index_constraint(Some("p"), PgFlavor::CockroachDB),
            Some(IndexConstraint::PrimaryKey)
        );
    }

    #[test]
    fn without_the_constraint_columns_the_index_query_selects_a_null_type() {
        // An engine whose `pg_constraint` lacks `conindid` would fail the
        // whole statement, emptying the schema tree. The degraded query must
        // not name it, and must still return the column the decoder reads.
        for bound in [true, false] {
            let degraded = index_query(bound, false);
            assert!(!degraded.contains("conindid"), "{degraded}");
            assert!(!degraded.contains("pg_constraint"), "{degraded}");
            assert!(
                degraded.contains("NULL::text AS constraint_type"),
                "{degraded}"
            );
            let rich = index_query(bound, true);
            assert!(rich.contains("con.contype AS constraint_type"), "{rich}");
            assert!(
                rich.contains("con ON con.conindid = ix.indexrelid"),
                "{rich}"
            );
            assert!(rich.contains("WHERE contype IN ('p', 'u', 'x')"), "{rich}");
        }
    }

    /// The source of [`index_query`], comment lines stripped and whitespace
    /// collapsed, so a reformat is not a failure and a comment cannot satisfy
    /// or trip an assertion.
    fn index_query_source() -> String {
        // `lines()` rather than a literal newline: a Windows checkout has
        // CRLF endings, and `trim_end` takes the `\r` off the closing brace.
        include_str!("postgres.rs")
            .lines()
            .skip_while(|line| !line.starts_with("fn index_query("))
            .take_while(|line| line.trim_end() != "}")
            .filter(|line| !line.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join(" ")
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
    }

    #[test]
    fn the_constraint_type_is_joined_once_not_looked_up_per_row() {
        // A scalar subquery on `pg_constraint` runs once per key-column row
        // and scans the whole catalog each time (1,478 ms against 21 ms on
        // 3,000 tables). The text the server receives must join it as one
        // derived table, and name it exactly once.
        for bound in [true, false] {
            let sql = index_query(bound, true)
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ");
            assert_eq!(sql.matches("pg_constraint").count(), 1, "{sql}");
            assert!(
                sql.contains(
                    "LEFT JOIN (SELECT conindid, min(contype::text) AS contype FROM pg_constraint"
                ),
                "{sql}"
            );
            assert!(!sql.contains("(SELECT con."), "{sql}");
            assert!(!sql.contains("LIMIT"), "{sql}");
        }
        // And the source carries no second, correlated spelling of it.
        let source = index_query_source();
        assert_eq!(source.matches("pg_constraint").count(), 1, "{source}");
        assert!(!source.contains("= ix.indexrelid AND"), "{source}");
    }
}
