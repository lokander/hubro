//! Integration tests for what a script leaves on its *session*: settings,
//! temp tables, attached databases, a current database, session-level locks.
//! None of it may outlive the run that set it, on any engine and down any of
//! the run paths (`run_script`'s atomic, sequential and read-only paths, and a
//! sequential script ending in a read, short or cut off by the row cap).
//!
//! Measured before the change, through `run_script` and then the pool: on
//! every Postgres-wire engine a `SET` (search_path, ROLE, statement_timeout,
//! `default_transaction_read_only`, …) reached later browsing and edits, and a
//! session advisory lock stayed held — the read-only path included; on SQL
//! Server `USE`, `SET CONTEXT_INFO`, `EXECUTE AS` and a session app lock did;
//! on SQLite every PRAGMA, temp table and `ATTACH` did, and
//! `PRAGMA locking_mode = EXCLUSIVE` locked every other connection out of the
//! file. A script typing `DISCARD ALL` broke the pooled connection outright:
//! sqlx's cached prepared statements were gone and the next query on it
//! failed. A run's connection is now closed when the run ends, so there is no
//! per-engine list of things to reset.
//!
//! The pools here hold **one** connection (SQL Server's hands idle clients back
//! last-in, first-out), so "later pooled work" is deterministically the
//! connection the script ran on whenever that connection went back to the
//! pool — a probe that cannot miss it by landing elsewhere.
//!
//! Every engine test skips unless its `HUBRO_*_TEST_URL` is set — see
//! `tests/CLAUDE.md`.

mod common;

use std::time::Duration;

use common::{script_outcome, FixtureDb};
use hubro::db::{run_script, split_statements, Capabilities, DbPool, Value, WriteProtection};

fn url(var: &str) -> Option<String> {
    match std::env::var(var) {
        Ok(url) => Some(url),
        Err(_) => {
            eprintln!("skipping session-state test: {var} not set");
            None
        }
    }
}

async fn run(pool: &DbPool, caps: Capabilities, sql: &str) -> String {
    let statements = split_statements(sql, pool.dialect());
    let outcome = run_script(pool, caps, &statements, |_| {}).await;
    match &outcome {
        Ok(_) => String::new(),
        Err(_) => script_outcome(&outcome),
    }
}

/// Runs `sql` as a script and fails the test unless it succeeded — a script
/// that failed before setting anything would make "nothing leaked" vacuous.
async fn run_ok(engine: &str, pool: &DbPool, caps: Capabilities, sql: &str) {
    let failed = run(pool, caps, sql).await;
    assert!(failed.is_empty(), "{engine}: {sql:?} failed: {failed}");
}

async fn one(pool: &DbPool, sql: &str) -> Value {
    let result = pool
        .query(sql)
        .await
        .unwrap_or_else(|e| panic!("{sql}: {e}"));
    result.rows[0][0].clone()
}

/// Polls `sql` until it returns `want` or a few seconds pass, returning the
/// last answer. Only for locks: a closed connection's session ends on the
/// server after the client has let go, so its locks are released a moment
/// later rather than before the run returns. A setting needs no wait — the
/// connection that held it is gone.
async fn eventually(pool: &DbPool, sql: &str, want: &Value) -> Value {
    let mut last = Value::Null;
    for _ in 0..50 {
        last = one(pool, sql).await;
        if &last == want {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    last
}

// ---------------------------------------------------------------------------
// Postgres-wire engines
// ---------------------------------------------------------------------------

/// What one Postgres-wire engine can be asked to do.
struct PgEngine {
    var: &'static str,
    /// Session-level advisory locks, asked only of PostgreSQL and the
    /// engines built on its server (TimescaleDB, Citus). The others' were not
    /// measured here: absent, or behind a server flag on YugabyteDB.
    advisory_locks: bool,
    /// `set_config`: absent on Materialize.
    set_config: bool,
}

const APP_NAME: &str = "SELECT current_setting('application_name')";

/// Fails with every leak `leaks` collected, so one run measures every path
/// rather than stopping at the first.
fn assert_nothing_leaked(engine: &str, leaks: &[String]) {
    assert!(
        leaks.is_empty(),
        "{engine}: a script's session state outlived its run:\n{}",
        leaks.join("\n")
    );
}

async fn assert_pg_session_state_stays_in_its_run(engine: &PgEngine, url: &str) {
    let var = engine.var;
    let pool = DbPool::open_postgres_single_session(url).await.unwrap();
    let caps = pool.backend_capabilities();
    let mut leaks = Vec::new();

    // Each run path: a lone statement (sequential), two statements (atomic
    // where the engine has transactions), the script's own transaction ending
    // in a read, and a read the row cap cuts short. Each sets its own value,
    // so a probe cannot pass by finding an earlier script's.
    let mut scripts = vec![
        (
            "hubro_leak_lone",
            "SET application_name = 'hubro_leak_lone'",
        ),
        (
            "hubro_leak_atomic",
            "SET application_name = 'hubro_leak_atomic'; SELECT 1",
        ),
    ];
    if caps.transactions {
        scripts.push((
            "hubro_leak_own_tx",
            "BEGIN; SET application_name = 'hubro_leak_own_tx'; COMMIT; SELECT 1",
        ));
    }
    if engine.set_config {
        scripts.push((
            "hubro_leak_capped",
            "SELECT set_config('application_name', 'hubro_leak_capped', false), g \
             FROM generate_series(1, 20000) g",
        ));
    }
    for (value, script) in scripts {
        run_ok(var, &pool, caps, script).await;
        if one(&pool, APP_NAME).await == Value::Text(value.to_string()) {
            leaks.push(format!(
                "{script:?} left its setting on the pooled connection"
            ));
        }
    }

    if engine.advisory_locks {
        let other = DbPool::open_postgres_single_session(url).await.unwrap();
        let marked = WriteProtection::ReadOnly.apply(caps);
        for (key, caps, script) in [
            (4_233_101, caps, "SELECT pg_advisory_lock(4233101)"),
            (
                4_233_102,
                caps,
                "SELECT pg_advisory_lock(4233102); SELECT 1",
            ),
            (4_233_103, marked, "SELECT pg_advisory_lock(4233103)"),
        ] {
            run_ok(var, &pool, caps, script).await;
            let taken = eventually(
                &other,
                &format!("SELECT CAST(pg_try_advisory_lock({key}) AS int)"),
                &Value::Integer(1),
            )
            .await;
            if taken == Value::Integer(1) {
                one(&other, &format!("SELECT pg_advisory_unlock({key})")).await;
            } else {
                leaks.push(format!(
                    "{script:?} (read-only: {}) left its lock held after the run",
                    !caps.mutate
                ));
            }
        }
        other.close().await;
    }
    // A script typing DISCARD ALL deallocated sqlx's cached prepared
    // statements, so the next cached query on that connection failed. Last,
    // because before the change it broke the pool's one connection.
    // CockroachDB refuses the statement, which is no leak.
    one(&pool, "SELECT 1").await;
    if run(&pool, caps, "DISCARD ALL").await.is_empty() {
        if let Err(e) = pool.query("SELECT 1").await {
            leaks.push(format!("a query after a script's DISCARD ALL failed: {e}"));
        }
    }
    pool.close().await;
    assert_nothing_leaked(var, &leaks);
}

#[tokio::test]
async fn postgres_script_session_state_stays_in_its_run() {
    let Some(url) = common::pg_test_url().await else {
        return;
    };
    let engine = PgEngine {
        var: "HUBRO_PG_TEST_URL",
        advisory_locks: true,
        set_config: true,
    };
    assert_pg_session_state_stays_in_its_run(&engine, &url).await;
}

#[tokio::test]
async fn timescale_script_session_state_stays_in_its_run() {
    let Some(url) = url("HUBRO_TIMESCALE_TEST_URL") else {
        return;
    };
    let engine = PgEngine {
        var: "HUBRO_TIMESCALE_TEST_URL",
        advisory_locks: true,
        set_config: true,
    };
    assert_pg_session_state_stays_in_its_run(&engine, &url).await;
}

#[tokio::test]
async fn citus_script_session_state_stays_in_its_run() {
    let Some(url) = url("HUBRO_CITUS_TEST_URL") else {
        return;
    };
    let engine = PgEngine {
        var: "HUBRO_CITUS_TEST_URL",
        advisory_locks: true,
        set_config: true,
    };
    assert_pg_session_state_stays_in_its_run(&engine, &url).await;
}

#[tokio::test]
async fn cockroach_script_session_state_stays_in_its_run() {
    let Some(url) = url("HUBRO_CRDB_TEST_URL") else {
        return;
    };
    let engine = PgEngine {
        var: "HUBRO_CRDB_TEST_URL",
        advisory_locks: false,
        set_config: true,
    };
    assert_pg_session_state_stays_in_its_run(&engine, &url).await;
}

#[tokio::test]
async fn yugabyte_script_session_state_stays_in_its_run() {
    let Some(url) = url("HUBRO_YUGABYTE_TEST_URL") else {
        return;
    };
    let engine = PgEngine {
        var: "HUBRO_YUGABYTE_TEST_URL",
        advisory_locks: false,
        set_config: true,
    };
    assert_pg_session_state_stays_in_its_run(&engine, &url).await;
}

#[tokio::test]
async fn materialize_script_session_state_stays_in_its_run() {
    let Some(url) = url("HUBRO_MATERIALIZE_TEST_URL") else {
        return;
    };
    let engine = PgEngine {
        var: "HUBRO_MATERIALIZE_TEST_URL",
        advisory_locks: false,
        set_config: false,
    };
    assert_pg_session_state_stays_in_its_run(&engine, &url).await;
}

#[tokio::test]
async fn risingwave_script_session_state_stays_in_its_run() {
    let Some(url) = url("HUBRO_RISINGWAVE_TEST_URL") else {
        return;
    };
    let engine = PgEngine {
        var: "HUBRO_RISINGWAVE_TEST_URL",
        advisory_locks: false,
        set_config: true,
    };
    assert_pg_session_state_stays_in_its_run(&engine, &url).await;
}

// ---------------------------------------------------------------------------
// SQL Server
// ---------------------------------------------------------------------------

#[tokio::test]
async fn sqlserver_script_session_state_stays_in_its_run() {
    let Some(url) = common::mssql_test_url().await else {
        return;
    };
    let var = "HUBRO_MSSQL_TEST_URL";
    let pool = DbPool::open_mssql(&url).await.unwrap();
    let caps = pool.backend_capabilities();
    let mut leaks = Vec::new();
    let context = "SELECT CAST(SUBSTRING(CONTEXT_INFO(), 1, 1) AS int)";

    for (marker, script) in [
        (1, "SET CONTEXT_INFO 0x01"),
        (2, "SET CONTEXT_INFO 0x02; SELECT 1"),
        (3, "BEGIN TRAN; SET CONTEXT_INFO 0x03; COMMIT; SELECT 1"),
        // Its own transaction, so sequential, ending in a read the row cap
        // cuts short.
        (
            4,
            "BEGIN TRAN; COMMIT; SET CONTEXT_INFO 0x04; SELECT TOP 20000 CAST(1 AS int) AS a \
             FROM sys.all_objects a CROSS JOIN sys.all_objects b",
        ),
    ] {
        run_ok(var, &pool, caps, script).await;
        // Idle clients go back last-in, first-out, so the probe lands on the
        // script's connection whenever it was returned.
        if one(&pool, context).await == Value::Integer(marker) {
            leaks.push(format!(
                "{script:?} left its CONTEXT_INFO on the pooled connection"
            ));
        }
    }
    run_ok(var, &pool, caps, "USE master").await;
    if one(&pool, "SELECT DB_NAME()").await == Value::Text("master".into()) {
        leaks.push("\"USE master\" left the pooled connection in another database".into());
    }

    let other = DbPool::open_mssql(&url).await.unwrap();
    let probe = "DECLARE @r int; \
                 EXEC @r = sp_getapplock @Resource = 'hubro_session_lock', \
                 @LockMode = 'Exclusive', @LockOwner = 'Session', @LockTimeout = 0; \
                 IF @r >= 0 EXEC sp_releaseapplock @Resource = 'hubro_session_lock', \
                 @LockOwner = 'Session'; \
                 SELECT CAST(CASE WHEN @r >= 0 THEN 1 ELSE 0 END AS int)";
    for script in [
        "EXEC sp_getapplock @Resource = 'hubro_session_lock', @LockMode = 'Exclusive', \
         @LockOwner = 'Session'",
        "EXEC sp_getapplock @Resource = 'hubro_session_lock', @LockMode = 'Exclusive', \
         @LockOwner = 'Session'; SELECT 1",
    ] {
        run_ok(var, &pool, caps, script).await;
        if eventually(&other, probe, &Value::Integer(1)).await != Value::Integer(1) {
            leaks.push(format!("{script:?} left its app lock held after the run"));
            // Released by hand, so the next script's lock is its own.
            one(
                &pool,
                "EXEC sp_releaseapplock @Resource = 'hubro_session_lock', \
                        @LockOwner = 'Session'; SELECT 1",
            )
            .await;
        }
    }
    other.close().await;
    pool.close().await;
    assert_nothing_leaked(var, &leaks);
}

// ---------------------------------------------------------------------------
// SQLite
// ---------------------------------------------------------------------------

/// The fixture on a pool of one connection, so later work is on the
/// connection the script ran on whenever it went back to the pool.
async fn sqlite_single_connection(fixture: &FixtureDb) -> DbPool {
    let options = sqlx::sqlite::SqliteConnectOptions::new().filename(fixture.path());
    DbPool::Sqlite(
        sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(options)
            .await
            .unwrap(),
    )
}

#[tokio::test]
async fn sqlite_script_session_state_stays_in_its_run() {
    let fixture = FixtureDb::with_sql("CREATE TABLE t (a INTEGER)").await;
    let pool = sqlite_single_connection(&fixture).await;
    let caps = pool.backend_capabilities();
    let engine = "SQLite";
    let mut leaks = Vec::new();

    run_ok(engine, &pool, caps, "PRAGMA foreign_keys = OFF").await;
    if one(&pool, "PRAGMA foreign_keys").await == Value::Integer(0) {
        leaks.push("\"PRAGMA foreign_keys = OFF\" reached the pooled connection".to_string());
    }

    for (script, name) in [
        ("CREATE TEMP TABLE leak_atomic (a); SELECT 1", "leak_atomic"),
        (
            "BEGIN; CREATE TEMP TABLE leak_own_tx (a); COMMIT; SELECT 1",
            "leak_own_tx",
        ),
    ] {
        run_ok(engine, &pool, caps, script).await;
        let sql = format!("SELECT count(*) FROM sqlite_temp_master WHERE name = '{name}'");
        if one(&pool, &sql).await != Value::Integer(0) {
            leaks.push(format!(
                "{script:?} left its temp table on the pooled connection"
            ));
        }
    }

    run_ok(engine, &pool, caps, "ATTACH ':memory:' AS leak_db").await;
    let attached = "SELECT count(*) FROM pragma_database_list WHERE name = 'leak_db'";
    if one(&pool, attached).await != Value::Integer(0) {
        leaks.push("\"ATTACH\" reached the pooled connection".to_string());
    }

    // Exclusive locking mode keeps the file locked from the connection's next
    // read on, so every other connection was locked out of it.
    run_ok(engine, &pool, caps, "PRAGMA locking_mode = EXCLUSIVE").await;
    one(&pool, "SELECT count(*) FROM t").await;
    let other = fixture.open().await;
    if let Err(e) = other.execute("INSERT INTO t VALUES (1)").await {
        leaks.push(format!(
            "\"PRAGMA locking_mode = EXCLUSIVE\" locked another connection out: {e}"
        ));
    }
    other.close().await;
    pool.close().await;
    assert_nothing_leaked(engine, &leaks);
}

// ---------------------------------------------------------------------------
// Cancelled runs
// ---------------------------------------------------------------------------

/// Starts `sql` as a script and cancels it the way the editor does — by
/// dropping the run — while its last statement still sleeps server-side, then
/// waits past the sleep, so a connection that was going back to the pool
/// would have.
async fn cancel_mid_sleep(pool: &DbPool, caps: Capabilities, sql: &str) {
    let statements = split_statements(sql, pool.dialect());
    let run = run_script(pool, caps, &statements, |_| {});
    assert!(
        tokio::time::timeout(Duration::from_millis(500), run)
            .await
            .is_err(),
        "{sql:?} should still be sleeping when it is cancelled"
    );
    tokio::time::sleep(Duration::from_millis(2500)).await;
}

/// A cancelled run's connection is closed too, on every path, and with it
/// whatever the script had left on the session. Cancelling skips the run's
/// own end, so this is the drop alone at work: a run's connections are marked
/// to close on drop when they are checked out (`acquire_for_run`), and
/// without that mark each of these went back to the pool. The atomic and
/// read-only paths are probed with a lock, because their transaction's
/// rollback undoes a `SET` but not a session advisory lock.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn postgres_cancelled_run_leaves_nothing_on_the_pool() {
    let Some(url) = common::pg_test_url().await else {
        return;
    };
    let pool = DbPool::open_postgres_single_session(&url).await.unwrap();
    let other = DbPool::open_postgres_single_session(&url).await.unwrap();
    let caps = pool.backend_capabilities();
    let marked = WriteProtection::ReadOnly.apply(caps);
    let mut leaks = Vec::new();

    // Sequential (the script's own BEGIN): a SET before it outlives nothing
    // but the session.
    cancel_mid_sleep(
        &pool,
        caps,
        "SET application_name = 'hubro_cancelled'; BEGIN; SELECT pg_sleep(2)",
    )
    .await;
    if one(&pool, APP_NAME).await == Value::Text("hubro_cancelled".into()) {
        leaks.push("a cancelled sequential run left its SET on the pooled connection".into());
    }

    for (key, caps, path) in [
        (4_233_201, caps, "atomic"),
        (4_233_202, marked, "read-only"),
    ] {
        cancel_mid_sleep(
            &pool,
            caps,
            &format!("SELECT pg_advisory_lock({key}); SELECT pg_sleep(2)"),
        )
        .await;
        let taken = eventually(
            &other,
            &format!("SELECT CAST(pg_try_advisory_lock({key}) AS int)"),
            &Value::Integer(1),
        )
        .await;
        if taken == Value::Integer(1) {
            one(&other, &format!("SELECT pg_advisory_unlock({key})")).await;
        } else {
            leaks.push(format!("a cancelled {path} run left its lock held"));
        }
    }
    other.close().await;
    pool.close().await;
    assert_nothing_leaked("HUBRO_PG_TEST_URL", &leaks);
}
