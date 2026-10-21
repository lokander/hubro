//! Integration tests for a connection the user marked read-only, against the
//! engines that have sequences — writes no keyword scan can see.
//!
//! `SELECT nextval('s')` and `SELECT setval('s', 100)` are reads to the
//! capability gate (`script_refusal`): no write keyword appears anywhere. So
//! are `SELECT set_config('default_transaction_read_only', 'off', false)` and
//! `BEGIN READ WRITE`, which undo a session-level read-only default — the
//! mechanism this backstop deliberately does *not* rely on. Each test checks
//! all three things that make that claim worth anything: the gate passes the
//! script, the script really writes on an unmarked connection, and on a
//! marked one the *engine* refuses it and the sequence does not move.
//!
//! The same backstop driven past the gate — DML and DDL handed straight to
//! the read-only path, standing in for a write a lexer mismatch hid — is the
//! `read_only_backstop` module in `src/db/script.rs`, which also covers SQLite,
//! Materialize and RisingWave (no sequences, and no write a `SELECT` can make).
//!
//! **SQL Server is the exception, and a test pins it as one.** T-SQL has no
//! read-only session or transaction mode, so a marked SQL Server connection
//! is protected by the gate alone, and `SELECT NEXT VALUE FOR s` advances the
//! sequence through it. If that test ever fails, SQL Server gained a
//! backstop and the documentation saying it has none is wrong.
//!
//! Every test skips unless its engine's `HUBRO_*_TEST_URL` is set — see
//! `tests/CLAUDE.md`.

mod common;

use hubro::db::{
    run_script, script_refusal, ConnectionRegistry, DbError, DbPool, Dialect, Value,
    WriteProtection,
};

fn url(var: &str) -> Option<String> {
    match std::env::var(var) {
        Ok(url) => Some(url),
        Err(_) => {
            eprintln!("skipping read-only test: {var} not set");
            None
        }
    }
}

fn stmts(sql: &[&str]) -> Vec<String> {
    sql.iter().map(|s| s.to_string()).collect()
}

async fn last_value(pool: &DbPool, sequence: &str) -> Value {
    let sql = match pool.dialect() {
        Dialect::SqlServer => format!(
            "SELECT CAST(current_value AS bigint) FROM sys.sequences WHERE name = '{sequence}'"
        ),
        Dialect::Postgres | Dialect::Sqlite => format!("SELECT last_value FROM {sequence}"),
    };
    let result = pool
        .query(&sql)
        .await
        .unwrap_or_else(|e| panic!("{sql}: {e}"));
    result.rows[0][0].clone()
}

/// The scripts no keyword scan sees as writes, for a sequence named `s`, each
/// with the value it sets the sequence to.
///
/// `setval` rather than `nextval` is what moves the sequence, because only an
/// explicit target is observable on every engine: YugabyteDB hands each
/// connection a cached block of values and reports the block in `last_value`,
/// so whether one `nextval` ran cannot be read back. Each target is distinct,
/// so no script can pass by finding the value its predecessor left.
fn invisible_writes(s: &str) -> Vec<(Vec<String>, i64)> {
    vec![
        (stmts(&[&format!("SELECT setval('{s}', 1001)")]), 1001),
        // Undoing a session default, then writing under it.
        (
            stmts(&[
                "SELECT set_config('default_transaction_read_only', 'off', false)",
                &format!("SELECT setval('{s}', 1002)"),
            ]),
            1002,
        ),
        // Overriding one: an explicit read-write transaction.
        (
            stmts(&[
                "BEGIN READ WRITE",
                &format!("SELECT setval('{s}', 1003)"),
                "COMMIT",
            ]),
            1003,
        ),
        // Ending hubro's own transaction from inside it, in the one statement
        // a splitter mismatch would hand over.
        (
            stmts(&[&format!("COMMIT; SELECT setval('{s}', 1004)")]),
            1004,
        ),
    ]
}

/// The whole claim for one Postgres-wire engine — see the module docs.
async fn assert_invisible_writes_are_refused(var: &str, pool: &DbPool) {
    let s = format!("hubro_ro_seq_{}", std::process::id());
    let _ = pool.execute(&format!("DROP SEQUENCE IF EXISTS {s}")).await;
    pool.execute(&format!("CREATE SEQUENCE {s}")).await.unwrap();
    let full = pool.backend_capabilities();
    let marked = WriteProtection::ReadOnly.apply(full);

    // `nextval` is refused too; only its effect is unobservable (see above).
    let nextval = stmts(&[&format!("SELECT nextval('{s}')")]);
    assert_eq!(script_refusal(marked, &nextval, Dialect::Postgres), None);
    let refused = run_script(pool, marked, &nextval, |_| {})
        .await
        .expect_err(&format!("{var}: nextval ran on a read-only connection"));
    assert!(
        matches!(refused.error, DbError::Query(_)),
        "{var}: {refused:?}"
    );
    run_script(pool, full, &nextval, |_| {})
        .await
        .unwrap_or_else(|e| panic!("{var}: nextval failed unmarked: {e:?}"));

    for (script, target) in invisible_writes(&s) {
        // The gate passes it: whatever stops it below is the engine.
        assert_eq!(
            script_refusal(marked, &script, Dialect::Postgres),
            None,
            "{var}: the gate refused {script:?}, so this proves nothing about the engine"
        );

        let refused = run_script(pool, marked, &script, |_| {})
            .await
            .expect_err(&format!("{var}: {script:?} ran on a read-only connection"));
        assert!(
            matches!(refused.error, DbError::Query(_)),
            "{var}: {script:?} must be refused by the server: {refused:?}"
        );
        assert_ne!(
            last_value(pool, &s).await,
            Value::Integer(target),
            "{var}: {script:?} moved the sequence on a read-only connection"
        );

        // And unmarked, it writes — the case is real. Not asked of the
        // scripts carrying their own transaction control: the multi-command
        // statement is refused by the extended protocol on any connection
        // (the half of the backstop that stops a mismatch smuggling the
        // COMMIT at all), and an unmarked script's own BEGIN … COMMIT runs
        // on the sequential path, which does not hold one connection across
        // statements — measured on PostgreSQL and CockroachDB, its BEGIN and
        // the statements after it reported different `pg_backend_pid()`s, so
        // the write can stay inside a transaction nothing commits.
        if script
            .iter()
            .any(|s| s.starts_with("BEGIN") || s.starts_with("COMMIT"))
        {
            continue;
        }
        run_script(pool, full, &script, |_| {})
            .await
            .unwrap_or_else(|e| panic!("{var}: {script:?} failed unmarked: {e:?}"));
        assert_eq!(
            last_value(pool, &s).await,
            Value::Integer(target),
            "{var}: {script:?} did not write even unmarked, so refusing it proves nothing"
        );
    }

    // Reads still run on the marked connection, through the same path.
    run_script(
        pool,
        marked,
        &stmts(&[
            &format!("SELECT last_value FROM {s}"),
            "SELECT count(*) FROM information_schema.tables",
        ]),
        |_| {},
    )
    .await
    .unwrap_or_else(|e| panic!("{var}: a read failed on a read-only connection: {e:?}"));

    pool.execute(&format!("DROP SEQUENCE {s}")).await.unwrap();
}

#[tokio::test]
async fn postgres_refuses_writes_the_gate_cannot_see() {
    let Some(url) = common::pg_test_url().await else {
        return;
    };
    let pool = DbPool::open_postgres(&url).await.unwrap();
    assert_invisible_writes_are_refused("HUBRO_PG_TEST_URL", &pool).await;
    pool.close().await;
}

#[tokio::test]
async fn timescale_refuses_writes_the_gate_cannot_see() {
    let Some(url) = url("HUBRO_TIMESCALE_TEST_URL") else {
        return;
    };
    let pool = DbPool::open_postgres(&url).await.unwrap();
    assert_invisible_writes_are_refused("HUBRO_TIMESCALE_TEST_URL", &pool).await;
    pool.close().await;
}

#[tokio::test]
async fn citus_refuses_writes_the_gate_cannot_see() {
    let Some(url) = url("HUBRO_CITUS_TEST_URL") else {
        return;
    };
    let pool = DbPool::open_postgres(&url).await.unwrap();
    assert_invisible_writes_are_refused("HUBRO_CITUS_TEST_URL", &pool).await;
    pool.close().await;
}

#[tokio::test]
async fn cockroach_refuses_writes_the_gate_cannot_see() {
    let Some(url) = url("HUBRO_CRDB_TEST_URL") else {
        return;
    };
    let pool = DbPool::open_postgres(&url).await.unwrap();
    assert_invisible_writes_are_refused("HUBRO_CRDB_TEST_URL", &pool).await;
    pool.close().await;
}

#[tokio::test]
async fn yugabyte_refuses_writes_the_gate_cannot_see() {
    let Some(url) = url("HUBRO_YUGABYTE_TEST_URL") else {
        return;
    };
    let pool = DbPool::open_postgres(&url).await.unwrap();
    assert_invisible_writes_are_refused("HUBRO_YUGABYTE_TEST_URL", &pool).await;
    pool.close().await;
}

/// The marking is read per run, so changing it on an open tab changes what the
/// engine allows on the next run — no reconnect, and nothing to re-apply,
/// because nothing about the pool changed. Driven the way the UI drives it:
/// through the registry's `set_protection` and the connection's effective
/// capabilities.
#[tokio::test]
async fn changing_the_marking_on_an_open_connection_takes_effect_on_the_next_run() {
    let Some(url) = common::pg_test_url().await else {
        return;
    };
    let pool = DbPool::open_postgres(&url).await.unwrap();
    let s = format!("hubro_ro_toggle_{}", std::process::id());
    let _ = pool.execute(&format!("DROP SEQUENCE IF EXISTS {s}")).await;
    pool.execute(&format!("CREATE SEQUENCE {s}")).await.unwrap();
    let mut registry = ConnectionRegistry::default();
    let id = registry.insert("pg", pool.clone(), WriteProtection::Open);
    let run = |registry: &ConnectionRegistry, target: i64| {
        let connection = registry.get(id).unwrap();
        let (pool, caps) = (connection.pool.clone(), connection.capabilities());
        let setval = stmts(&[&format!("SELECT setval('{s}', {target})")]);
        async move { run_script(&pool, caps, &setval, |_| {}).await }
    };

    run(&registry, 10).await.expect("unmarked, setval runs");
    assert_eq!(last_value(&pool, &s).await, Value::Integer(10));
    registry.set_protection(id, WriteProtection::ReadOnly);
    let refused = run(&registry, 20)
        .await
        .expect_err("marked read-only, the server must refuse setval");
    assert!(matches!(refused.error, DbError::Query(_)), "{refused:?}");
    assert_eq!(last_value(&pool, &s).await, Value::Integer(10));
    registry.set_protection(id, WriteProtection::Confirm);
    run(&registry, 30)
        .await
        .expect("unmarked again, setval runs again — nothing stuck to the pool");
    assert_eq!(last_value(&pool, &s).await, Value::Integer(30));

    pool.execute(&format!("DROP SEQUENCE {s}")).await.unwrap();
    pool.close().await;
}

/// The documented limit, measured: SQL Server has no read-only mode, so a
/// write the gate cannot see goes straight through a marked connection.
#[tokio::test]
async fn sql_server_has_no_backstop_behind_the_gate() {
    let Some(url) = common::mssql_test_url().await else {
        return;
    };
    let pool = DbPool::open_mssql(&url).await.unwrap();
    let s = format!("hubro_ro_seq_{}", std::process::id());
    let _ = pool.execute(&format!("DROP SEQUENCE IF EXISTS {s}")).await;
    pool.execute(&format!("CREATE SEQUENCE {s} START WITH 1"))
        .await
        .unwrap();
    let marked = WriteProtection::ReadOnly.apply(pool.backend_capabilities());
    let script = stmts(&[&format!("SELECT NEXT VALUE FOR {s}")]);
    assert_eq!(script_refusal(marked, &script, Dialect::SqlServer), None);
    // `current_value` reads the start value until the first draw, so draw
    // once unmarked to make the next one visible.
    run_script(&pool, pool.backend_capabilities(), &script, |_| {})
        .await
        .unwrap();

    let before = last_value(&pool, &s).await;
    run_script(&pool, marked, &script, |_| {})
        .await
        .expect("nothing on SQL Server refuses what the gate passed");
    assert_ne!(
        last_value(&pool, &s).await,
        before,
        "the sequence did not move — SQL Server refused a write on a marked \
         connection, so the documented limit is out of date"
    );

    pool.execute(&format!("DROP SEQUENCE {s}")).await.unwrap();
    pool.close().await;
}
