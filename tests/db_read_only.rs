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
//! **Browsing is held to the same claim.** The grid's page, its count, a cell
//! fetch, a table's exact count and an export run SQL hubro built, but what
//! that SQL reads can write: a view over `setval()` (or over a function that
//! inserts) fires on every read of it. On a marked Postgres-wire connection
//! each of those reads runs in the same always-rolled-back read-only
//! transaction, so the engine refuses the write; `browsing_*` below pins it
//! per engine, with the same three checks, and that browsing an ordinary
//! table still works there — Materialize and RisingWave included.
//!
//! Every test skips unless its engine's `HUBRO_*_TEST_URL` is set — see
//! `tests/CLAUDE.md`.

mod common;

use hubro::db::{
    run_script, script_refusal, Capabilities, ConnectionRegistry, DbError, DbPool, Dialect,
    ExportFormat, PageRequest, PgFlavor, RestOfRead, RowIdentity, RowLocator, StatementOutcome,
    TableMeta, Value, WriteProtection,
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

/// The gate's other side on a real marked connection: a read that calls
/// `replace()` inside a CTE (or under `EXPLAIN`) runs, as the same read without
/// the CTE always did. Matching the word `replace` alone charged it as a
/// `REPLACE` statement and refused it before anything reached the server.
#[tokio::test]
async fn a_marked_postgres_connection_runs_a_read_that_calls_replace() {
    let Some(url) = common::pg_test_url().await else {
        return;
    };
    let pool = DbPool::open_postgres(&url).await.unwrap();
    let marked = WriteProtection::ReadOnly.apply(pool.backend_capabilities());
    let script = stmts(&[
        "SELECT replace('a-b', '-', ' ') AS n",
        "WITH x AS (SELECT replace('a-b', '-', ' ') AS n) SELECT n FROM x",
        "EXPLAIN WITH x AS (SELECT replace('a-b', '-', ' ') AS n) SELECT n FROM x",
    ]);
    assert_eq!(script_refusal(marked, &script, Dialect::Postgres), None);
    let mut rows = Vec::new();
    run_script(&pool, marked, &script, |r| rows.push(r))
        .await
        .unwrap_or_else(|e| panic!("a read was refused on a marked connection: {e:?}"));
    assert_eq!(rows.len(), 3);
    for result in &rows[..2] {
        let StatementOutcome::Rows(result) = &result.outcome else {
            panic!("{result:?}");
        };
        assert_eq!(result.rows, vec![vec![Value::Text("a b".into())]]);
    }
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

/// A marked run whose read-only connection cannot be opened fails, rather than
/// falling back to the ordinary path — which would run the script on the
/// pool's own, writable connections.
///
/// The SQLite file is renamed away under an open pool: the pool's connection
/// keeps the inode (unix semantics, hence the `cfg`) and still reads, while
/// opening a new, read-only connection by name fails.
#[cfg(unix)]
#[tokio::test]
async fn a_marked_run_that_cannot_open_its_read_only_connection_fails_closed() {
    let fixture =
        common::FixtureDb::with_sql("CREATE TABLE t (a INTEGER); INSERT INTO t VALUES (1)").await;
    let pool = fixture.open().await;
    let moved = fixture.path().with_extension("moved");
    std::fs::rename(fixture.path(), &moved).unwrap();
    let script = stmts(&["SELECT count(*) FROM t"]);

    let marked = WriteProtection::ReadOnly.apply(pool.backend_capabilities());
    assert_eq!(script_refusal(marked, &script, Dialect::Sqlite), None);
    let refused = run_script(&pool, marked, &script, |_| {})
        .await
        .expect_err("a marked run must not fall back to the pool's own connections");
    assert!(matches!(refused.error, DbError::Connect(_)), "{refused:?}");

    // The fallback it refused is real: unmarked, the pool still runs it, on
    // the connection it already holds. sqlx hands a finished connection back
    // to the pool from a spawned task, so an acquire that races that return
    // opens a new connection by name instead, and that fails just as the
    // read-only open did; CI lost this race under load on macOS and Linux.
    // So wait for the connection to come back rather than reading a lost
    // race as "no fallback".
    let mut lost_races = 0;
    while let Err(err) = run_script(&pool, pool.backend_capabilities(), &script, |_| {}).await {
        lost_races += 1;
        assert!(
            lost_races < 50,
            "the pool's own connection still reads the renamed file: {err:?}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }

    std::fs::rename(&moved, fixture.path()).unwrap();
    pool.close().await;
}

/// A statement that ends its own connection — the gate passes
/// `pg_terminate_backend`, which acts on another process — fails with its own
/// error, and the next marked run gets a working connection that is still
/// read-only: the failed rollback left nothing behind in the pool.
#[tokio::test]
async fn a_marked_connection_recovers_from_a_statement_that_kills_it() {
    let Some(url) = common::pg_test_url().await else {
        return;
    };
    let pool = DbPool::open_postgres(&url).await.unwrap();
    let marked = WriteProtection::ReadOnly.apply(pool.backend_capabilities());
    let s = format!("hubro_ro_kill_{}", std::process::id());
    let _ = pool.execute(&format!("DROP SEQUENCE IF EXISTS {s}")).await;
    pool.execute(&format!("CREATE SEQUENCE {s}")).await.unwrap();

    let killed = run_script(
        &pool,
        marked,
        &stmts(&["SELECT pg_terminate_backend(pg_backend_pid())"]),
        |_| {},
    )
    .await
    .expect_err("a statement that ends its connection cannot succeed");
    assert!(
        killed.error.to_string().contains("terminat"),
        "the statement's own error, not the rollback's: {killed:?}"
    );

    for _ in 0..4 {
        run_script(&pool, marked, &stmts(&["SELECT 1"]), |_| {})
            .await
            .expect("the pool recovers");
        let refused = run_script(
            &pool,
            marked,
            &stmts(&[&format!("SELECT setval('{s}', 77)")]),
            |_| {},
        )
        .await
        .expect_err("and is still read-only");
        assert!(matches!(refused.error, DbError::Query(_)), "{refused:?}");
    }
    assert_ne!(last_value(&pool, &s).await, Value::Integer(77));

    pool.execute(&format!("DROP SEQUENCE {s}")).await.unwrap();
    pool.close().await;
}

/// The other documented limit, measured: Citus's `run_command_on_*`
/// functions run their command over a connection of Citus's own, outside the
/// read-only transaction, so a marked connection whose login may call them
/// still writes — to this very database, through `run_command_on_coordinator`.
/// Same class as `dblink_exec`; if this test fails, Citus started refusing and
/// the documentation saying it does not is out of date.
#[tokio::test]
async fn citus_writes_through_run_command_on_coordinator_despite_the_marking() {
    let Some(url) = url("HUBRO_CITUS_TEST_URL") else {
        return;
    };
    let pool = DbPool::open_postgres(&url).await.unwrap();
    let t = format!("hubro_ro_citus_{}", std::process::id());
    let _ = pool.execute(&format!("DROP TABLE IF EXISTS {t}")).await;
    pool.execute(&format!("CREATE TABLE {t} (a int)"))
        .await
        .unwrap();
    let marked = WriteProtection::ReadOnly.apply(pool.backend_capabilities());
    let script = stmts(&[&format!(
        "SELECT run_command_on_coordinator($$INSERT INTO {t} VALUES (1)$$)"
    )]);
    assert_eq!(script_refusal(marked, &script, Dialect::Postgres), None);

    run_script(&pool, marked, &script, |_| {})
        .await
        .expect("nothing in the read-only transaction refuses a write made elsewhere");
    let rows = pool
        .query(&format!("SELECT count(*) FROM {t}"))
        .await
        .unwrap();
    assert_eq!(
        rows.rows,
        vec![vec![Value::Integer(1)]],
        "the row is not there — Citus refused a write on a marked connection, so the \
         documented limit is out of date"
    );

    pool.execute(&format!("DROP TABLE {t}")).await.unwrap();
    pool.close().await;
}

/// Far more rows than the row cap, generated as they are sent (in the select
/// list a set-returning function streams; in `FROM` it would be materialized
/// first). Read to its end it takes about 45 s on localhost, so a run that
/// waits for it is told apart from one that does not by the clock alone.
const LONG_READ: &str = "SELECT generate_series(1, 200000000)";

/// How many sessions are still running [`LONG_READ`]. The pattern is split
/// so that this question does not match itself.
async fn sessions_running_the_long_read(pool: &DbPool) -> Value {
    let running = "query LIKE '%generate' || '_series(1, 200000000)%'";
    let sql = match pool.pg_flavor() {
        Some(PgFlavor::CockroachDB) => {
            format!("SELECT count(*) FROM [SHOW CLUSTER STATEMENTS] WHERE {running}")
        }
        _ => format!(
            "SELECT count(*) FROM pg_stat_activity \
             WHERE pid <> pg_backend_pid() AND state = 'active' AND {running}"
        ),
    };
    pool.query(&sql).await.unwrap().rows[0][0].clone()
}

/// A marked connection's read that the row cap cut short is not read to its
/// end — neither as the script's last statement, whose transaction is ended
/// by closing the connection, nor before another, where it is cancelled —
/// and the server stops running it. Its transaction was rolled back either
/// way, and the engine refused any write in it, so the rest of the result
/// could change nothing. Measured before: 4.2 s per 20M rows on localhost,
/// waited for in both places.
async fn assert_a_capped_read_is_not_read_to_its_end(pool: &DbPool) {
    let marked = WriteProtection::ReadOnly.apply(pool.backend_capabilities());
    for script in [vec![LONG_READ], vec![LONG_READ, "SELECT 1"]] {
        let started = std::time::Instant::now();
        let mut results = Vec::new();
        run_script(pool, marked, &stmts(&script), |r| results.push(r))
            .await
            .unwrap_or_else(|e| panic!("{script:?}: {e:?}"));
        let took = started.elapsed();
        assert!(
            took < std::time::Duration::from_secs(15),
            "{script:?} waited {took:?} for the rest of its read"
        );
        assert!(results[0].truncated, "{script:?}");
        assert_eq!(results[0].rest, RestOfRead::Stopped, "{script:?}");
        if let Some(after) = results.get(1) {
            match &after.outcome {
                StatementOutcome::Rows(rows) => {
                    assert_eq!(rows.rows, vec![vec![Value::Integer(1)]], "{script:?}")
                }
                other => panic!("{script:?}: {other:?}"),
            }
        }
        let mut running = sessions_running_the_long_read(pool).await;
        for _ in 0..50 {
            if running == Value::Integer(0) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            running = sessions_running_the_long_read(pool).await;
        }
        assert_eq!(
            running,
            Value::Integer(0),
            "{script:?}: the server is still sending the rest"
        );
    }
}

// Multi-threaded, as the app's runtime is.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_marked_postgres_connection_does_not_read_a_capped_read_to_its_end() {
    let Some(url) = common::pg_test_url().await else {
        return;
    };
    let pool = DbPool::open_postgres(&url).await.unwrap();
    assert_a_capped_read_is_not_read_to_its_end(&pool).await;
    pool.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_marked_cockroach_connection_does_not_read_a_capped_read_to_its_end() {
    let Some(url) = url("HUBRO_CRDB_TEST_URL") else {
        return;
    };
    let pool = DbPool::open_postgres(&url).await.unwrap();
    assert_a_capped_read_is_not_read_to_its_end(&pool).await;
    pool.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_marked_yugabyte_connection_does_not_read_a_capped_read_to_its_end() {
    let Some(url) = url("HUBRO_YUGABYTE_TEST_URL") else {
        return;
    };
    let pool = DbPool::open_postgres(&url).await.unwrap();
    assert_a_capped_read_is_not_read_to_its_end(&pool).await;
    pool.close().await;
}

/// One of the reads the grid and the schema pane make on a table — each one
/// a separate path through `DbPool`, so each is checked on its own.
#[derive(Debug, Clone, Copy)]
enum Browse {
    Page,
    Count,
    TableCount,
    Cell,
    Export,
}

const BROWSES: [Browse; 5] = [
    Browse::Page,
    Browse::Count,
    Browse::TableCount,
    Browse::Cell,
    Browse::Export,
];

/// Runs one browse of `table` with these effective capabilities. The cell
/// fetch addresses the row by an `id` column of value 1, which every object
/// these tests browse has — a view has no key of its own, and the path under
/// test is the read, not the addressing.
async fn browse(
    pool: &DbPool,
    caps: Capabilities,
    table: &TableMeta,
    how: Browse,
) -> Result<String, DbError> {
    let request = PageRequest {
        schema: table.schema.clone(),
        table: table.name.clone(),
        limit: 100,
        offset: 0,
        sort: None,
        filter: None,
        extra_key_column: None,
    };
    match how {
        Browse::Page => pool
            .fetch_page_bounded(caps, &request, &table.columns, &["id"])
            .await
            .map(|page| format!("{:?}", page.result.rows)),
        Browse::Count => pool.count_rows(caps, &request).await.map(|n| n.to_string()),
        Browse::TableCount => pool
            .count_table_rows(caps, table)
            .await
            .map(|n| n.to_string()),
        Browse::Cell => {
            let identity = RowIdentity::PrimaryKey {
                columns: vec!["id".into()],
            };
            let locator = RowLocator {
                identity_values: vec![Value::Integer(1)],
            };
            let column = &table
                .columns
                .iter()
                .find(|c| c.name != "id")
                .expect("a column besides id")
                .name;
            pool.fetch_cell(caps, table, &identity, &locator, column)
                .await
                .map(|cell| format!("{:?}", cell.value))
        }
        Browse::Export => {
            let (sql, params) = request.select_sql(pool.dialect());
            let mut out = Vec::new();
            pool.export(caps, &sql, &params, ExportFormat::Csv, &mut out)
                .await
                .map(|_| String::from_utf8(out).unwrap())
        }
    }
}

async fn object_named(pool: &DbPool, name: &str) -> TableMeta {
    pool.introspect()
        .await
        .unwrap()
        .into_iter()
        .find(|t| t.name == name)
        .unwrap_or_else(|| panic!("introspection did not list {name}"))
}

/// Browsing a view that writes when read, on one engine with sequences: the
/// marked read is refused by the engine and the sequence does not move, and
/// the same read unmarked does move it — so the refusal is what stopped it.
async fn assert_browsing_does_not_write(var: &str, pool: &DbPool) {
    let s = format!("hubro_ro_browse_seq_{}", std::process::id());
    let v = format!("hubro_ro_browse_view_{}", std::process::id());
    let _ = pool.execute(&format!("DROP VIEW IF EXISTS {v}")).await;
    let _ = pool.execute(&format!("DROP SEQUENCE IF EXISTS {s}")).await;
    pool.execute(&format!("CREATE SEQUENCE {s}")).await.unwrap();
    // `setval` rather than `nextval`, for YugabyteDB — see `invisible_writes`.
    // Except on CockroachDB, whose stored view loses `setval`'s second
    // argument (measured: "unknown signature: setval(oid)" on every read of
    // it), and which reports one `nextval` exactly.
    let write = match pool.pg_flavor() {
        Some(PgFlavor::CockroachDB) => format!("nextval('{s}')"),
        _ => format!("setval('{s}', 3001)"),
    };
    pool.execute(&format!("CREATE VIEW {v} AS SELECT 1 AS id, {write} AS n"))
        .await
        .unwrap();
    let view = object_named(pool, &v).await;
    let full = pool.backend_capabilities();
    let marked = WriteProtection::ReadOnly.apply(full);
    // Each browse starts from here, and the view moves the sequence off it.
    let reset = Value::Integer(1);

    let mut wrote = Vec::new();
    for how in BROWSES {
        // Unmarked first: whether this browse writes at all is the engine's
        // business — CockroachDB plans `COUNT(*)` over the view without
        // calling `nextval` — and a browse that does not write has nothing
        // to refuse.
        pool.query(&format!("SELECT setval('{s}', 1)"))
            .await
            .unwrap();
        browse(pool, full, &view, how)
            .await
            .unwrap_or_else(|e| panic!("{var}: {how:?} failed unmarked: {e:?}"));
        let writes = last_value(pool, &s).await != reset;

        pool.query(&format!("SELECT setval('{s}', 1)"))
            .await
            .unwrap();
        let marked_read = browse(pool, marked, &view, how).await;
        assert_eq!(
            last_value(pool, &s).await,
            reset,
            "{var}: {how:?} moved the sequence on a read-only connection"
        );
        if writes {
            let refused =
                marked_read.expect_err(&format!("{var}: {how:?} of a writing view ran marked"));
            // The engine's own refusal, not any failure: a view the engine
            // cannot run at all would pass a looser check.
            assert!(
                refused.to_string().contains("read-only transaction"),
                "{var}: {how:?} must be refused as a write in a read-only transaction: \
                 {refused:?}"
            );
            wrote.push(how);
        } else {
            marked_read.unwrap_or_else(|e| {
                panic!("{var}: {how:?} writes nothing, yet failed marked: {e:?}")
            });
        }
    }
    eprintln!("{var}: writes when browsed unmarked: {wrote:?}");
    // The page is the browse every engine runs the view for; without it,
    // nothing above was refused and the test would prove nothing.
    assert!(
        wrote.iter().any(|how| matches!(how, Browse::Page)),
        "{var}: browsing the view did not write even unmarked: {wrote:?}"
    );

    pool.execute(&format!("DROP VIEW {v}")).await.unwrap();
    pool.execute(&format!("DROP SEQUENCE {s}")).await.unwrap();
}

/// Browsing an ordinary table on a marked connection reads what the same
/// browse reads unmarked, on every Postgres-wire engine — the read-only
/// transaction must not cost a working grid anywhere.
async fn assert_marked_browsing_reads(var: &str, pool: &DbPool) {
    let t = format!("hubro_ro_browse_t_{}", std::process::id());
    let _ = pool.execute(&format!("DROP TABLE IF EXISTS {t}")).await;
    pool.execute(&format!("CREATE TABLE {t} (id int, body text)"))
        .await
        .unwrap();
    pool.execute(&format!(
        "INSERT INTO {t} VALUES (1, 'one'), (2, 'two'), (3, 'three')"
    ))
    .await
    .unwrap();
    if pool.pg_flavor() == Some(PgFlavor::RisingWave) {
        // RisingWave makes a write visible to reads only once flushed.
        pool.execute("FLUSH").await.unwrap();
    }
    let table = object_named(pool, &t).await;
    let full = pool.backend_capabilities();
    let marked = WriteProtection::ReadOnly.apply(full);
    for how in BROWSES {
        let unmarked = browse(pool, full, &table, how)
            .await
            .unwrap_or_else(|e| panic!("{var}: {how:?} failed unmarked: {e:?}"));
        let read = browse(pool, marked, &table, how)
            .await
            .unwrap_or_else(|e| panic!("{var}: {how:?} failed on a marked connection: {e:?}"));
        // Page and export rows come back in no promised order.
        let sorted = |text: &str| {
            let mut chars: Vec<char> = text.chars().collect();
            chars.sort_unstable();
            chars
        };
        assert_eq!(sorted(&read), sorted(&unmarked), "{var}: {how:?}");
    }
    pool.execute(&format!("DROP TABLE {t}")).await.unwrap();
}

#[tokio::test]
async fn browsing_a_writing_view_on_a_marked_postgres_connection_is_refused() {
    let Some(url) = common::pg_test_url().await else {
        return;
    };
    let pool = DbPool::open_postgres(&url).await.unwrap();
    assert_browsing_does_not_write("HUBRO_PG_TEST_URL", &pool).await;
    assert_marked_browsing_reads("HUBRO_PG_TEST_URL", &pool).await;
    pool.close().await;
}

#[tokio::test]
async fn browsing_a_writing_view_on_a_marked_timescale_connection_is_refused() {
    let Some(url) = url("HUBRO_TIMESCALE_TEST_URL") else {
        return;
    };
    let pool = DbPool::open_postgres(&url).await.unwrap();
    assert_browsing_does_not_write("HUBRO_TIMESCALE_TEST_URL", &pool).await;
    assert_marked_browsing_reads("HUBRO_TIMESCALE_TEST_URL", &pool).await;
    pool.close().await;
}

#[tokio::test]
async fn browsing_a_writing_view_on_a_marked_citus_connection_is_refused() {
    let Some(url) = url("HUBRO_CITUS_TEST_URL") else {
        return;
    };
    let pool = DbPool::open_postgres(&url).await.unwrap();
    assert_browsing_does_not_write("HUBRO_CITUS_TEST_URL", &pool).await;
    assert_marked_browsing_reads("HUBRO_CITUS_TEST_URL", &pool).await;
    pool.close().await;
}

#[tokio::test]
async fn browsing_a_writing_view_on_a_marked_cockroach_connection_is_refused() {
    let Some(url) = url("HUBRO_CRDB_TEST_URL") else {
        return;
    };
    let pool = DbPool::open_postgres(&url).await.unwrap();
    assert_browsing_does_not_write("HUBRO_CRDB_TEST_URL", &pool).await;
    assert_marked_browsing_reads("HUBRO_CRDB_TEST_URL", &pool).await;
    pool.close().await;
}

#[tokio::test]
async fn browsing_a_writing_view_on_a_marked_yugabyte_connection_is_refused() {
    let Some(url) = url("HUBRO_YUGABYTE_TEST_URL") else {
        return;
    };
    let pool = DbPool::open_postgres(&url).await.unwrap();
    assert_browsing_does_not_write("HUBRO_YUGABYTE_TEST_URL", &pool).await;
    assert_marked_browsing_reads("HUBRO_YUGABYTE_TEST_URL", &pool).await;
    pool.close().await;
}

/// Materialize and RisingWave have no sequences and no function a view can
/// write through, so there is nothing to refuse — only that browsing inside
/// their read-only transactions still works.
#[tokio::test]
async fn browsing_on_a_marked_materialize_connection_still_reads() {
    let Some(url) = url("HUBRO_MATERIALIZE_TEST_URL") else {
        return;
    };
    let pool = DbPool::open_postgres(&url).await.unwrap();
    assert_marked_browsing_reads("HUBRO_MATERIALIZE_TEST_URL", &pool).await;
    pool.close().await;
}

#[tokio::test]
async fn browsing_on_a_marked_risingwave_connection_still_reads() {
    let Some(url) = url("HUBRO_RISINGWAVE_TEST_URL") else {
        return;
    };
    let pool = DbPool::open_postgres(&url).await.unwrap();
    assert_marked_browsing_reads("HUBRO_RISINGWAVE_TEST_URL", &pool).await;
    pool.close().await;
}

/// Why SQLite browsing needs no read-only open: a `SELECT` there writes only
/// through a function an application registered or an extension it loaded,
/// and hubro registers none and lets no statement load one. If this fails,
/// `DbPool::browse_with`'s SQLite arm has lost its reason.
#[tokio::test]
async fn a_sqlite_read_cannot_load_an_extension() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.db");
    std::fs::File::create(&path).unwrap();
    let pool = DbPool::open_sqlite(&path).await.unwrap();
    let refused = pool
        .query("SELECT load_extension('hubro_no_such_extension')")
        .await
        .expect_err("load_extension ran");
    assert!(
        refused.to_string().contains("not authorized"),
        "refused for some other reason than being disabled: {refused}"
    );
    pool.close().await;
}

/// How many notifications `listener` receives before half a second passes
/// without one.
async fn notifications_delivered(listener: &mut sqlx::postgres::PgListener) -> usize {
    let mut got = 0;
    while let Ok(Ok(_)) =
        tokio::time::timeout(std::time::Duration::from_millis(500), listener.recv()).await
    {
        got += 1;
    }
    got
}

/// A marked connection's browse is rolled back, never committed: the one
/// effect a read-only transaction allows and a commit would make real is a
/// notification, which `pg_notify` queues and only a commit delivers. A view
/// calling it is browsed every way, marked — nothing arrives — and the same
/// browse unmarked delivers, so the listener is listening.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn browsing_a_marked_postgres_connection_delivers_no_notification() {
    let Some(url) = common::pg_test_url().await else {
        return;
    };
    let pool = DbPool::open_postgres(&url).await.unwrap();
    let channel = format!("hubro_ro_browse_{}", std::process::id());
    let v = format!("hubro_ro_notify_view_{}", std::process::id());
    let _ = pool.execute(&format!("DROP VIEW IF EXISTS {v}")).await;
    pool.execute(&format!(
        "CREATE VIEW {v} AS SELECT 1 AS id, pg_notify('{channel}', 'x')::text AS n"
    ))
    .await
    .unwrap();
    let view = object_named(&pool, &v).await;
    let full = pool.backend_capabilities();
    let marked = WriteProtection::ReadOnly.apply(full);
    let mut listener = sqlx::postgres::PgListener::connect(&url).await.unwrap();
    listener.listen(&channel).await.unwrap();

    for how in BROWSES {
        browse(&pool, marked, &view, how)
            .await
            .unwrap_or_else(|e| panic!("{how:?} failed marked: {e:?}"));
        assert_eq!(
            notifications_delivered(&mut listener).await,
            0,
            "{how:?}: a marked browse's transaction was committed"
        );
    }
    browse(&pool, full, &view, Browse::Page).await.unwrap();
    assert_eq!(
        notifications_delivered(&mut listener).await,
        1,
        "unmarked, the page's notification must arrive, or the marked case proves nothing"
    );
    pool.execute(&format!("DROP VIEW {v}")).await.unwrap();
    pool.close().await;
}
