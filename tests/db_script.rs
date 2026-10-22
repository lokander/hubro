//! Integration tests for script execution: `DbPool::execute` affected-row
//! counts, atomic multi-statement runs (a mid-script failure rolls the whole
//! batch back), and Postgres error position enrichment.
//!
//! Postgres tests need a running server (Docker only, per CLAUDE.md) and are
//! skipped unless `HUBRO_PG_TEST_URL` is set — see tests/db_postgres.rs.

mod common;

use common::FixtureDb;
use hubro::db::{
    run_script, split_statements, DbError, DbPool, OpenTransaction, Rollback, ScriptDone,
    StatementOutcome, StatementResult, Value,
};

async fn pg_url() -> Option<String> {
    common::pg_test_url().await
}

/// Runs a script (split from one SQL text) collecting per-statement results.
async fn collect_script(
    pool: &DbPool,
    sql: &str,
) -> (
    Vec<StatementResult>,
    Result<hubro::db::ScriptDone, hubro::db::ScriptError>,
) {
    let statements = split_statements(sql, pool.dialect());
    let mut results = Vec::new();
    let outcome = run_script(pool, pool.backend_capabilities(), &statements, |r| {
        results.push(r)
    })
    .await;
    (results, outcome)
}

#[tokio::test]
async fn sqlite_execute_reports_rows_affected() {
    let fixture = FixtureDb::with_sql("CREATE TABLE t (a INTEGER, b TEXT)").await;
    let pool = fixture.open().await;

    assert_eq!(
        pool.execute("INSERT INTO t (a, b) VALUES (1, 'x'), (2, 'y'), (3, 'z')")
            .await
            .unwrap(),
        3
    );
    assert_eq!(
        pool.execute("UPDATE t SET b = 'q' WHERE a >= 2")
            .await
            .unwrap(),
        2
    );
    assert_eq!(pool.execute("DELETE FROM t WHERE a = 1").await.unwrap(), 1);
    // Errors surface as query errors, not panics.
    assert!(matches!(
        pool.execute("UPDATE nope SET a = 1").await,
        Err(DbError::Query(_))
    ));

    pool.close().await;
}

#[tokio::test]
async fn sqlite_script_rolls_back_the_whole_batch_on_error() {
    let fixture = FixtureDb::with_sql("").await;
    let pool = fixture.open().await;

    let (results, outcome) = collect_script(
        &pool,
        "CREATE TABLE t (a INTEGER); \
         INSERT INTO t VALUES (1), (2); \
         SELECT * FROM missing_table; \
         INSERT INTO t VALUES (3)",
    )
    .await;

    // Progress is reported for the statements that ran before the failure.
    assert_eq!(results.len(), 2);
    assert_eq!(results[0].outcome, StatementOutcome::Affected(0));
    assert_eq!(results[1].outcome, StatementOutcome::Affected(2));
    let err = outcome.unwrap_err();
    assert_eq!(err.statement_index, 2);
    assert_eq!(err.preview, "SELECT * FROM missing_table");
    assert!(matches!(err.error, DbError::Query(_)));
    assert_eq!(
        err.rollback,
        Rollback::Full,
        "a multi-statement script wraps atomically, schema changes included"
    );

    // Atomic wrapping: the whole batch rolled back, so even the CREATE TABLE
    // is gone — the table never came into existence.
    let tables = pool
        .query("SELECT name FROM sqlite_master WHERE type = 'table' AND name = 't'")
        .await
        .unwrap();
    assert!(
        tables.rows.is_empty(),
        "table t should not exist after rollback, got {:?}",
        tables.rows
    );

    pool.close().await;
}

#[tokio::test]
async fn sqlite_script_commits_every_statement_on_success() {
    let fixture = FixtureDb::with_sql("").await;
    let pool = fixture.open().await;

    let (_results, outcome) = collect_script(
        &pool,
        "CREATE TABLE t (a INTEGER); \
         INSERT INTO t VALUES (1), (2); \
         INSERT INTO t VALUES (3)",
    )
    .await;
    outcome.unwrap();

    // All statements committed: the rows are there after the run.
    let rows = pool.query("SELECT a FROM t ORDER BY a").await.unwrap();
    assert_eq!(
        rows.rows,
        vec![
            vec![Value::Integer(1)],
            vec![Value::Integer(2)],
            vec![Value::Integer(3)]
        ]
    );

    pool.close().await;
}

#[tokio::test]
async fn sqlite_script_with_vacuum_runs_sequentially_and_keeps_prior_effects() {
    // VACUUM can't run inside a transaction, so the script falls back to
    // sequential autocommit — a later failure leaves earlier effects in place.
    let fixture = FixtureDb::with_sql("CREATE TABLE t (a INTEGER)").await;
    let pool = fixture.open().await;

    let (results, outcome) = collect_script(
        &pool,
        "INSERT INTO t VALUES (1), (2); \
         VACUUM; \
         SELECT * FROM missing_table",
    )
    .await;

    assert_eq!(results.len(), 2); // insert + vacuum ran; the select failed
    let err = outcome.unwrap_err();
    assert_eq!(err.statement_index, 2);
    assert_eq!(
        err.rollback,
        Rollback::None,
        "a VACUUM-containing script runs sequentially, not atomically"
    );

    // The insert persisted (no wrapping transaction to undo it).
    let rows = pool.query("SELECT a FROM t ORDER BY a").await.unwrap();
    assert_eq!(
        rows.rows,
        vec![vec![Value::Integer(1)], vec![Value::Integer(2)]]
    );

    pool.close().await;
}

#[tokio::test]
async fn sqlite_script_mixes_reads_and_writes() {
    let fixture = FixtureDb::with_sql("CREATE TABLE t (a INTEGER)").await;
    let pool = fixture.open().await;

    let (results, outcome) = collect_script(
        &pool,
        "-- seed; the comment semicolon must not split\n\
         INSERT INTO t VALUES (1), (2), (3);\n\
         SELECT a FROM t WHERE a > 1 ORDER BY a;\n\
         DELETE FROM t WHERE a = 'kept;literal' OR a = 3;",
    )
    .await;

    outcome.unwrap();
    assert_eq!(results.len(), 3);
    assert_eq!(results[0].outcome, StatementOutcome::Affected(3));
    match &results[1].outcome {
        StatementOutcome::Rows(r) => {
            assert_eq!(
                r.rows,
                vec![vec![Value::Integer(2)], vec![Value::Integer(3)]]
            );
        }
        other => panic!("expected rows, got {other:?}"),
    }
    assert_eq!(results[2].outcome, StatementOutcome::Affected(1));

    pool.close().await;
}

#[tokio::test]
async fn postgres_execute_reports_rows_affected() {
    let Some(url) = pg_url().await else { return };
    let pool = DbPool::open_postgres(&url).await.unwrap();

    pool.execute("DROP TABLE IF EXISTS exec_counts")
        .await
        .unwrap();
    assert_eq!(
        pool.execute("CREATE TABLE exec_counts (a integer, b text)")
            .await
            .unwrap(),
        0
    );
    assert_eq!(
        pool.execute("INSERT INTO exec_counts (a, b) VALUES (1, 'x'), (2, 'y'), (3, 'z')")
            .await
            .unwrap(),
        3
    );
    assert_eq!(
        pool.execute("UPDATE exec_counts SET b = 'q' WHERE a >= 2")
            .await
            .unwrap(),
        2
    );
    assert_eq!(
        pool.execute("DELETE FROM exec_counts WHERE a = 1")
            .await
            .unwrap(),
        1
    );

    pool.close().await;
}

#[tokio::test]
async fn postgres_errors_carry_line_and_column_positions() {
    let Some(url) = pg_url().await else { return };
    let pool = DbPool::open_postgres(&url).await.unwrap();

    // The undefined column starts at line 2, column 3.
    let err = pool.query("SELECT\n  bad_column").await.unwrap_err();
    let message = err.to_string();
    assert!(
        message.contains("bad_column") && message.contains("(line 2, column 3)"),
        "unexpected message: {message}"
    );

    // The execute path is enriched too.
    let err = pool
        .execute("UPDATE nowhere_at_all SET x = 1")
        .await
        .unwrap_err();
    let message = err.to_string();
    assert!(
        message.contains("(line 1, column 8)"),
        "unexpected message: {message}"
    );

    pool.close().await;
}

#[tokio::test]
async fn postgres_script_rolls_back_the_whole_batch_on_error() {
    let Some(url) = pg_url().await else { return };
    let pool = DbPool::open_postgres(&url).await.unwrap();

    pool.execute("DROP TABLE IF EXISTS script_seq")
        .await
        .unwrap();
    let (results, outcome) = collect_script(
        &pool,
        "CREATE TABLE script_seq (a integer); \
         INSERT INTO script_seq VALUES (1), (2); \
         INSERT INTO script_seq VALUES ('not a number'); \
         INSERT INTO script_seq VALUES (3)",
    )
    .await;

    assert_eq!(results.len(), 2);
    assert_eq!(results[1].outcome, StatementOutcome::Affected(2));
    let err = outcome.unwrap_err();
    assert_eq!(err.statement_index, 2);
    assert_eq!(
        err.rollback,
        Rollback::Full,
        "a multi-statement script wraps atomically, schema changes included"
    );

    // Atomic wrapping: the whole batch rolled back, so the CREATE TABLE is
    // undone and the relation does not exist.
    let count = pool
        .query(
            "SELECT count(*)::int FROM information_schema.tables \
             WHERE table_name = 'script_seq'",
        )
        .await
        .unwrap();
    assert_eq!(
        count.rows,
        vec![vec![Value::Integer(0)]],
        "script_seq should not exist after rollback"
    );

    pool.close().await;
}

#[tokio::test]
async fn postgres_script_commits_every_statement_on_success() {
    let Some(url) = pg_url().await else { return };
    let pool = DbPool::open_postgres(&url).await.unwrap();

    pool.execute("DROP TABLE IF EXISTS script_ok")
        .await
        .unwrap();
    let (_results, outcome) = collect_script(
        &pool,
        "CREATE TABLE script_ok (a integer); \
         INSERT INTO script_ok VALUES (1), (2); \
         INSERT INTO script_ok VALUES (3)",
    )
    .await;
    outcome.unwrap();

    let rows = pool
        .query("SELECT a FROM script_ok ORDER BY a")
        .await
        .unwrap();
    assert_eq!(
        rows.rows,
        vec![
            vec![Value::Integer(1)],
            vec![Value::Integer(2)],
            vec![Value::Integer(3)]
        ]
    );

    pool.execute("DROP TABLE script_ok").await.unwrap();
    pool.close().await;
}

#[tokio::test]
async fn postgres_script_with_vacuum_runs_sequentially_and_keeps_prior_effects() {
    let Some(url) = pg_url().await else { return };
    let pool = DbPool::open_postgres(&url).await.unwrap();

    // VACUUM can't run inside a transaction block, so the script falls back to
    // sequential autocommit — a later failure leaves earlier effects in place.
    pool.execute("DROP TABLE IF EXISTS script_vac")
        .await
        .unwrap();
    pool.execute("CREATE TABLE script_vac (a integer)")
        .await
        .unwrap();

    let (results, outcome) = collect_script(
        &pool,
        "INSERT INTO script_vac VALUES (1), (2); \
         VACUUM script_vac; \
         SELECT * FROM missing_relation",
    )
    .await;

    assert_eq!(results.len(), 2); // insert + vacuum ran; the select failed
    let err = outcome.unwrap_err();
    assert_eq!(err.statement_index, 2);
    assert_eq!(
        err.rollback,
        Rollback::None,
        "a VACUUM-containing script runs sequentially on Postgres too"
    );

    let rows = pool
        .query("SELECT a FROM script_vac ORDER BY a")
        .await
        .unwrap();
    assert_eq!(
        rows.rows,
        vec![vec![Value::Integer(1)], vec![Value::Integer(2)]]
    );

    pool.execute("DROP TABLE script_vac").await.unwrap();
    pool.close().await;
}

/// A trigger body's inner `;` used to split the trigger into fragments that
/// are each an error (and the trailing `END` read as a COMMIT). Now the whole
/// trigger reaches SQLite as one statement, and the script still wraps
/// atomically: the trigger's leading `CREATE` is not transaction control.
#[tokio::test]
async fn sqlite_script_creates_a_trigger_with_a_multi_statement_body() {
    let fixture = FixtureDb::with_sql("").await;
    let pool = fixture.open().await;

    let script = "CREATE TABLE a (x INTEGER); \
         CREATE TABLE total (n INTEGER); \
         CREATE TABLE log (x INTEGER); \
         INSERT INTO total VALUES (0); \
         CREATE TRIGGER bump AFTER INSERT ON a BEGIN \
           UPDATE total SET n = CASE WHEN new.x > 100 THEN n ELSE n + new.x END; \
           INSERT INTO log VALUES (new.x); \
         END; \
         INSERT INTO a VALUES (5), (7); \
         SELECT (SELECT n FROM total), (SELECT group_concat(x) FROM log)";
    assert_eq!(split_statements(script, pool.dialect()).len(), 7);
    let (results, outcome) = collect_script(&pool, script).await;
    outcome.unwrap();
    assert_eq!(results.len(), 7);
    match &results[6].outcome {
        // Both statements of the body ran for each inserted row.
        StatementOutcome::Rows(rows) => assert_eq!(
            rows.rows,
            vec![vec![Value::Integer(12), Value::Text("5,7".into())]]
        ),
        other => panic!("expected rows from the SELECT, got {other:?}"),
    }

    // A failure after the trigger rolls it back with everything else.
    let (_results, outcome) = collect_script(
        &pool,
        "CREATE TRIGGER gone AFTER INSERT ON a BEGIN SELECT 1; SELECT 2; END; \
         SELECT * FROM missing_table",
    )
    .await;
    let err = outcome.unwrap_err();
    assert_eq!(err.statement_index, 1);
    assert_eq!(err.rollback, Rollback::Full);
    let triggers = pool
        .query("SELECT name FROM sqlite_master WHERE type = 'trigger' ORDER BY name")
        .await
        .unwrap();
    assert_eq!(triggers.rows, vec![vec![Value::Text("bump".into())]]);

    pool.close().await;
}

/// Postgres 14+ SQL-standard bodies (`BEGIN ATOMIC … END`) are not
/// dollar-quoted, so their inner `;` used to split the function apart.
#[tokio::test]
async fn postgres_script_creates_a_begin_atomic_function() {
    let Some(url) = pg_url().await else { return };
    let pool = DbPool::open_postgres(&url).await.unwrap();
    pool.execute("DROP PROCEDURE IF EXISTS script_atomic_fill")
        .await
        .unwrap();
    pool.execute("DROP TABLE IF EXISTS script_atomic")
        .await
        .unwrap();

    let script = "CREATE TABLE script_atomic (a integer); \
         CREATE PROCEDURE script_atomic_fill() LANGUAGE sql BEGIN ATOMIC \
           INSERT INTO script_atomic VALUES (1); \
           INSERT INTO script_atomic VALUES (CASE WHEN true THEN 2 END); \
         END; \
         CALL script_atomic_fill(); \
         SELECT a FROM script_atomic ORDER BY a";
    assert_eq!(split_statements(script, pool.dialect()).len(), 4);
    let (results, outcome) = collect_script(&pool, script).await;
    outcome.unwrap();
    match &results[3].outcome {
        StatementOutcome::Rows(rows) => assert_eq!(
            rows.rows,
            vec![vec![Value::Integer(1)], vec![Value::Integer(2)]]
        ),
        other => panic!("expected rows from the SELECT, got {other:?}"),
    }

    pool.execute("DROP PROCEDURE script_atomic_fill")
        .await
        .unwrap();
    pool.execute("DROP TABLE script_atomic").await.unwrap();
    pool.close().await;
}

/// Runs each script twice against a table holding one row: on a fully
/// capable connection, where it must delete the row (proving the engine runs
/// the hidden DELETE, so the case is real), and on a read-only one, where the
/// gate must refuse it before anything runs.
async fn assert_hidden_writes_are_refused(pool: &DbPool, table: &str, scripts: &[&str]) {
    let count = format!("SELECT COUNT(*) FROM {table}");
    for script in scripts {
        pool.execute(&format!("DELETE FROM {table}")).await.unwrap();
        pool.execute(&format!("INSERT INTO {table} VALUES (1)"))
            .await
            .unwrap();
        let statements = split_statements(script, pool.dialect());
        let full = pool.backend_capabilities();
        let _ = run_script(pool, full, &statements, |_| {}).await;
        let rows = pool.query(&count).await.unwrap();
        assert_eq!(
            rows.rows,
            vec![vec![Value::Integer(0)]],
            "{script:?} should delete the row when allowed"
        );

        pool.execute(&format!("INSERT INTO {table} VALUES (1)"))
            .await
            .unwrap();
        let outcome = run_script(pool, full.read_only(), &statements, |_| {}).await;
        match outcome {
            Err(err) => assert!(
                matches!(err.error, DbError::Unsupported(_)),
                "{script:?} must be refused by the gate, not fail at the server: {err:?}"
            ),
            Ok(_) => panic!("{script:?} ran on a read-only connection, split as {statements:?}"),
        }
        let rows = pool.query(&count).await.unwrap();
        assert_eq!(rows.rows, vec![vec![Value::Integer(1)]], "{script:?}");
    }
}

/// SQLite reads `` `…` `` and `[…]` as quoted identifiers, never nests block
/// comments, has no dollar quotes, and runs every statement in a string
/// (measured: each script below deleted the row through a "read"). A lexer
/// that disagreed on any of these read the script as one SELECT that needed
/// nothing.
#[tokio::test]
async fn sqlite_scripts_cannot_hide_a_write_from_the_gate() {
    let fixture = FixtureDb::with_sql("CREATE TABLE t (a INTEGER)").await;
    let pool = fixture.open().await;
    assert_hidden_writes_are_refused(
        &pool,
        "t",
        &[
            "SELECT 1 AS `'`; DELETE FROM t; SELECT 1 AS `'`",
            "SELECT 1 AS [']; DELETE FROM t; SELECT 1 AS [']",
            "SELECT 1 /* /* */ ; DELETE FROM t; /* */",
            "SELECT 1 AS x$y$; DELETE FROM t; SELECT 1 AS x$y$",
        ],
    )
    .await;
    pool.close().await;
}

/// Postgres ends a line comment at a bare CR, reads backslash escapes in
/// `E'…'`, continues one across a newline, never opens a dollar quote inside
/// a name, and takes non-ASCII dollar-quote tags. Each script below is one
/// statement — a data-modifying CTE — that a lexer disagreeing on any of
/// these read as a read with nothing to charge.
#[tokio::test]
async fn postgres_scripts_cannot_hide_a_write_from_the_gate() {
    let Some(url) = pg_url().await else { return };
    let pool = DbPool::open_postgres(&url).await.unwrap();
    pool.execute("DROP TABLE IF EXISTS script_hidden")
        .await
        .unwrap();
    pool.execute("CREATE TABLE script_hidden (a integer)")
        .await
        .unwrap();
    assert_hidden_writes_are_refused(
        &pool,
        "script_hidden",
        &[
            "WITH d AS (SELECT 1 AS a) --c\r, e AS (DELETE FROM script_hidden RETURNING a) \
             SELECT * FROM d",
            "--c\rWITH e AS (DELETE FROM script_hidden RETURNING a)\nSELECT 1",
            "WITH d AS (SELECT E'\\'' AS a), e AS (DELETE FROM script_hidden RETURNING a) \
             SELECT 1 --'",
            "WITH d AS (SELECT E'a'\n'\\'' AS a), e AS (DELETE FROM script_hidden RETURNING a) \
             SELECT 1 --'",
            "WITH d AS (SELECT 1 AS x$y$), e AS (DELETE FROM script_hidden RETURNING a) \
             SELECT 1 AS x$y$",
            "WITH d AS (SELECT $é$'$é$ AS a), e AS (DELETE FROM script_hidden RETURNING a) \
             SELECT 1 --'",
        ],
    )
    .await;
    pool.execute("DROP TABLE script_hidden").await.unwrap();
    pool.close().await;
}

/// SQL Server ends a line comment at a bare CR, has no dollar quotes, and
/// ends a number (hex too) at its first character that cannot continue it;
/// it also needs no `;` between statements, so each script below is a SELECT
/// and a write that a lexer disagreeing on any of these read as one SELECT.
#[tokio::test]
async fn sqlserver_scripts_cannot_hide_a_write_from_the_gate() {
    let Some(url) = common::mssql_test_url().await else {
        return;
    };
    let pool = DbPool::open_mssql(&url).await.unwrap();
    pool.execute("DROP TABLE IF EXISTS dbo.script_hidden")
        .await
        .unwrap();
    pool.execute("CREATE TABLE dbo.script_hidden (a int)")
        .await
        .unwrap();
    assert_hidden_writes_are_refused(
        &pool,
        "dbo.script_hidden",
        &[
            "SELECT 1 AS a --c\rDELETE FROM dbo.script_hidden",
            "SELECT 1DELETE FROM dbo.script_hidden",
            "SELECT $1DELETE FROM dbo.script_hidden",
            "SELECT €1DELETE FROM dbo.script_hidden",
            "SELECT .5DELETE FROM dbo.script_hidden",
            "SELECT.5DELETE FROM dbo.script_hidden",
            "SELECT DISTINCT.5DELETE FROM dbo.script_hidden",
            "SELECT $.5DELETE FROM dbo.script_hidden",
            "SELECT 0xATRUNCATE TABLE dbo.script_hidden",
            "SELECT 0x1FTRUNCATE TABLE dbo.script_hidden",
            "SELECT 1 AS x$y$ DELETE FROM dbo.script_hidden SELECT 1 AS x$y$",
        ],
    )
    .await;
    pool.execute("DROP TABLE dbo.script_hidden").await.unwrap();
    pool.close().await;
}

/// SQLite reads `$name(…)`, `:name(…)` and `@name(…)` as one variable whose
/// parentheses hide a quote or `/*`, which this lexer does not model. It
/// needs no modelling while sqlx-sqlite refuses every named parameter before
/// running anything (measured: "parameters with non-integer names are not
/// currently supported"), so these scripts delete nothing even on a fully
/// capable connection. If a driver upgrade starts binding named parameters,
/// this fails, and the lexer has to learn SQLite's variable token.
#[tokio::test]
async fn sqlite_tcl_variables_never_reach_a_later_statement() {
    let fixture = FixtureDb::with_sql("CREATE TABLE t (a INTEGER); INSERT INTO t VALUES (1)").await;
    let pool = fixture.open().await;
    for script in [
        "SELECT $a('); DELETE FROM t; SELECT 'x'",
        "SELECT :a(/*); DELETE FROM t; SELECT 1 /* */",
        "SELECT @a('); DELETE FROM t; SELECT 'x'",
    ] {
        let statements = split_statements(script, pool.dialect());
        let outcome = run_script(&pool, pool.backend_capabilities(), &statements, |_| {}).await;
        assert!(outcome.is_err(), "{script:?} should fail before running");
        let rows = pool.query("SELECT COUNT(*) FROM t").await.unwrap();
        assert_eq!(rows.rows, vec![vec![Value::Integer(1)]], "{script:?}");
    }
    pool.close().await;
}

/// A NUL never reaches the driver: measured, sqlx-sqlite spins at full CPU
/// forever on a statement holding one. The gate refuses it up front, on a
/// fully capable connection too, and promptly.
#[tokio::test]
async fn sqlite_scripts_holding_a_nul_are_refused_not_run() {
    let fixture = FixtureDb::with_sql("CREATE TABLE t (a INTEGER); INSERT INTO t VALUES (1)").await;
    let pool = fixture.open().await;
    let statements = split_statements("SELECT 1;\0DELETE FROM t", pool.dialect());
    let outcome = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        run_script(&pool, pool.backend_capabilities(), &statements, |_| {}),
    )
    .await
    .expect("a NUL must be refused, not handed to the driver");
    let err = outcome.expect_err("a NUL must be refused");
    assert_eq!(
        err.error,
        DbError::Unsupported(hubro::db::NUL_IN_SCRIPT.to_string())
    );
    pool.close().await;
}

// A script that manages its own transaction runs outside hubro's, on the
// sequential path, and that path once took a pooled connection per statement:
// `BEGIN` opened a transaction on one session while the statements after it
// ran on others. Measured before the fix, on all four engines: a script's
// `BEGIN; INSERT; ROLLBACK` left its row, a transaction the script left open
// went back to the pool and swallowed later unrelated statements (on SQLite,
// later edits failed with "cannot start a transaction within a transaction"
// or "database is locked"), and on Postgres a transaction an error had
// aborted failed every query that landed on its connection afterwards.

/// Slow enough that four at once need four pooled connections.
const SQLITE_SLOW: &str = "WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM c \
                           WHERE x < 300000) SELECT count(*) FROM c";

async fn count(pool: &DbPool, sql: &str) -> Value {
    pool.query(sql).await.unwrap().rows[0][0].clone()
}

/// Every pooled connection can take a guarded write and commit it: none is
/// left inside a transaction a script abandoned. Six writes, so each of the
/// several connections [`common::open_several_connections`] opened is used.
async fn sqlite_pool_takes_edits(pool: &DbPool, table: &str) {
    for n in 0..6 {
        pool.execute_checked(
            &format!("INSERT INTO {table} VALUES (?)"),
            &[Value::Integer(n)],
            1,
        )
        .await
        .unwrap_or_else(|e| panic!("edit {n} failed on a pooled connection: {e}"));
    }
}

#[tokio::test]
async fn sqlite_script_keeps_its_own_transaction_on_one_connection() {
    let fixture = FixtureDb::with_sql("CREATE TABLE own_tx (a INTEGER)").await;
    let pool = fixture.open().await;
    common::open_several_connections(&pool, SQLITE_SLOW).await;

    let (_, outcome) =
        collect_script(&pool, "BEGIN; INSERT INTO own_tx VALUES (1); ROLLBACK").await;
    assert_eq!(outcome, Ok(ScriptDone::default()));
    assert_eq!(
        count(&pool, "SELECT count(*) FROM own_tx").await,
        Value::Integer(0),
        "the script's ROLLBACK must undo its INSERT"
    );

    let (_, outcome) = collect_script(&pool, "BEGIN; INSERT INTO own_tx VALUES (2); COMMIT").await;
    assert_eq!(outcome, Ok(ScriptDone::default()));
    let fresh = fixture.open().await;
    assert_eq!(
        count(&fresh, "SELECT count(*) FROM own_tx").await,
        Value::Integer(1),
        "the script's COMMIT must commit its INSERT"
    );
    fresh.close().await;
    pool.close().await;
}

#[tokio::test]
async fn sqlite_script_left_inside_its_transaction_is_rolled_back_and_says_so() {
    let fixture =
        FixtureDb::with_sql("CREATE TABLE left_open (a INTEGER); CREATE TABLE edits (a INTEGER)")
            .await;
    let pool = fixture.open().await;
    common::open_several_connections(&pool, SQLITE_SLOW).await;

    let (results, outcome) = collect_script(&pool, "BEGIN; INSERT INTO left_open VALUES (1)").await;
    assert_eq!(results.len(), 2, "both statements ran");
    assert_eq!(
        outcome,
        Ok(ScriptDone {
            rolled_back: Some(OpenTransaction {
                schema_changes_kept: false
            }),
            connection_closed: false,
        }),
        "a script that ends inside its own transaction must say it was rolled back"
    );

    sqlite_pool_takes_edits(&pool, "edits").await;
    let fresh = fixture.open().await;
    assert_eq!(
        count(&fresh, "SELECT count(*) FROM left_open").await,
        Value::Integer(0),
        "the uncommitted INSERT must be gone, not parked on a pooled connection"
    );
    assert_eq!(
        count(&fresh, "SELECT count(*) FROM edits").await,
        Value::Integer(6)
    );
    fresh.close().await;
    pool.close().await;
}

#[tokio::test]
async fn sqlite_script_failing_inside_its_transaction_rolls_it_back() {
    let fixture =
        FixtureDb::with_sql("CREATE TABLE failed_tx (a INTEGER); CREATE TABLE edits (a INTEGER)")
            .await;
    let pool = fixture.open().await;
    common::open_several_connections(&pool, SQLITE_SLOW).await;

    let (_, outcome) = collect_script(
        &pool,
        "BEGIN; INSERT INTO failed_tx VALUES (1); SELECT * FROM no_such_table; COMMIT",
    )
    .await;
    let err = outcome.unwrap_err();
    assert_eq!(err.statement_index, 2);
    assert_eq!(
        err.rollback,
        Rollback::OpenTransaction(OpenTransaction {
            schema_changes_kept: false
        })
    );

    sqlite_pool_takes_edits(&pool, "edits").await;
    assert_eq!(
        count(&pool, "SELECT count(*) FROM failed_tx").await,
        Value::Integer(0)
    );
    pool.close().await;
}

/// Each pooled connection's open transaction, if any, as seen from inside it:
/// `pg_current_xact_id_if_assigned()` is NULL in a query's own implicit
/// transaction and the id of the abandoned one when the query runs inside a
/// transaction that wrote. Four at once, so each of the pool's four
/// connections answers; a connection inside an aborted transaction fails.
async fn pg_open_transactions(pool: &DbPool) -> Vec<String> {
    let reads: Vec<_> = (0..4)
        .map(|_| pool.query("SELECT pg_current_xact_id_if_assigned()::text, pg_sleep(0.2)"))
        .collect();
    futures_util::future::join_all(reads)
        .await
        .into_iter()
        .filter_map(|read| match read {
            Ok(result) if result.rows[0][0] == Value::Null => None,
            Ok(result) => Some(format!("open transaction {:?}", result.rows[0][0])),
            Err(e) => Some(e.to_string()),
        })
        .collect()
}

#[tokio::test]
async fn postgres_script_keeps_its_own_transaction_on_one_session() {
    let Some(url) = pg_url().await else { return };
    let pool = DbPool::open_postgres(&url).await.unwrap();
    pool.execute("DROP TABLE IF EXISTS own_tx").await.unwrap();
    pool.execute("CREATE TABLE own_tx (a integer)")
        .await
        .unwrap();
    common::open_several_connections(&pool, "SELECT pg_sleep(0.2)").await;

    let (results, outcome) = collect_script(
        &pool,
        "SELECT pg_backend_pid(); BEGIN; SELECT pg_backend_pid(); \
         INSERT INTO own_tx VALUES (1); SELECT pg_backend_pid(); ROLLBACK; \
         SELECT pg_backend_pid()",
    )
    .await;
    assert_eq!(outcome, Ok(ScriptDone::default()));
    let pids: Vec<&Value> = results
        .iter()
        .filter_map(|r| match &r.outcome {
            StatementOutcome::Rows(rows) => Some(&rows.rows[0][0]),
            StatementOutcome::Affected(_) => None,
        })
        .collect();
    assert_eq!(pids.len(), 4);
    assert!(
        pids.iter().all(|pid| *pid == pids[0]),
        "every statement of the script must run in one session, got {pids:?}"
    );
    assert_eq!(
        count(&pool, "SELECT count(*) FROM own_tx").await,
        Value::Integer(0),
        "the script's ROLLBACK must undo its INSERT"
    );
    pool.execute("DROP TABLE own_tx").await.unwrap();
    pool.close().await;
}

#[tokio::test]
async fn postgres_script_left_inside_its_transaction_is_rolled_back_and_says_so() {
    let Some(url) = pg_url().await else { return };
    let pool = DbPool::open_postgres(&url).await.unwrap();
    pool.execute("DROP TABLE IF EXISTS left_open")
        .await
        .unwrap();
    pool.execute("CREATE TABLE left_open (a integer)")
        .await
        .unwrap();
    common::open_several_connections(&pool, "SELECT pg_sleep(0.2)").await;

    let (_, outcome) = collect_script(&pool, "BEGIN; INSERT INTO left_open VALUES (1)").await;
    assert_eq!(
        outcome,
        Ok(ScriptDone {
            rolled_back: Some(OpenTransaction {
                schema_changes_kept: false
            }),
            connection_closed: false,
        })
    );
    assert_eq!(pg_open_transactions(&pool).await, Vec::<String>::new());
    assert_eq!(
        count(&pool, "SELECT count(*) FROM left_open").await,
        Value::Integer(0)
    );

    // A script that ends its own transaction leaves nothing to report.
    let (_, outcome) =
        collect_script(&pool, "BEGIN; INSERT INTO left_open VALUES (2); COMMIT").await;
    assert_eq!(outcome, Ok(ScriptDone::default()));
    assert_eq!(
        count(&pool, "SELECT count(*) FROM left_open").await,
        Value::Integer(1)
    );
    pool.execute("DROP TABLE left_open").await.unwrap();
    pool.close().await;
}

#[tokio::test]
async fn postgres_script_failing_inside_its_transaction_rolls_it_back() {
    let Some(url) = pg_url().await else { return };
    let pool = DbPool::open_postgres(&url).await.unwrap();
    pool.execute("DROP TABLE IF EXISTS failed_tx")
        .await
        .unwrap();
    pool.execute("CREATE TABLE failed_tx (a integer)")
        .await
        .unwrap();
    common::open_several_connections(&pool, "SELECT pg_sleep(0.2)").await;

    let (_, outcome) = collect_script(
        &pool,
        "BEGIN; INSERT INTO failed_tx VALUES (1); SELECT * FROM no_such_table; COMMIT",
    )
    .await;
    let err = outcome.unwrap_err();
    assert_eq!(err.statement_index, 2);
    assert_eq!(
        err.rollback,
        Rollback::OpenTransaction(OpenTransaction {
            schema_changes_kept: false
        })
    );
    // No connection is left in the aborted transaction, failing whatever
    // query lands on it next.
    assert_eq!(pg_open_transactions(&pool).await, Vec::<String>::new());
    assert_eq!(
        count(&pool, "SELECT count(*) FROM failed_tx").await,
        Value::Integer(0)
    );
    pool.execute("DROP TABLE failed_tx").await.unwrap();
    pool.close().await;
}

#[tokio::test]
async fn postgres_cancelled_script_does_not_return_its_transaction_to_the_pool() {
    // The editor cancels a run by dropping it mid-statement, so the run never
    // reaches the end that would roll the script's transaction back. The
    // connection must then be closed rather than returned: returned, it went
    // back inside the transaction once its statement finished.
    let Some(url) = pg_url().await else { return };
    let pool = DbPool::open_postgres(&url).await.unwrap();
    pool.execute("DROP TABLE IF EXISTS cancelled_tx")
        .await
        .unwrap();
    pool.execute("CREATE TABLE cancelled_tx (a integer)")
        .await
        .unwrap();

    let statements = split_statements(
        "BEGIN; INSERT INTO cancelled_tx VALUES (1); SELECT pg_sleep(1.5)",
        pool.dialect(),
    );
    let run = run_script(&pool, pool.backend_capabilities(), &statements, |_| {});
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(500), run)
            .await
            .is_err(),
        "the run should still be sleeping when it is cancelled"
    );
    // Past the sleep, so a connection that was going back to the pool would
    // have.
    tokio::time::sleep(std::time::Duration::from_millis(2500)).await;

    assert_eq!(pg_open_transactions(&pool).await, Vec::<String>::new());
    assert_eq!(
        count(&pool, "SELECT count(*) FROM cancelled_tx").await,
        Value::Integer(0)
    );
    pool.execute("DROP TABLE cancelled_tx").await.unwrap();
    pool.close().await;
}

#[tokio::test]
async fn sqlite_cancelled_script_does_not_return_its_transaction_to_the_pool() {
    // A run cancelled mid-statement never reaches its end, so its connection
    // must be closed: returned, it carried the script's transaction — and
    // SQLite's write lock with it — back into the pool, where later edits
    // failed with "database is locked".
    let fixture = FixtureDb::with_sql(
        "CREATE TABLE cancelled_tx (a INTEGER); CREATE TABLE edits (a INTEGER)",
    )
    .await;
    let pool = fixture.open().await;
    common::open_several_connections(&pool, SQLITE_SLOW).await;
    let statements = split_statements(
        &format!(
            "BEGIN; INSERT INTO cancelled_tx VALUES (1); {SQLITE_SLOW}; {SQLITE_SLOW}; \
             {SQLITE_SLOW}; {SQLITE_SLOW}"
        ),
        pool.dialect(),
    );
    let run = run_script(&pool, pool.backend_capabilities(), &statements, |_| {});
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(150), run)
            .await
            .is_err(),
        "the run should still be busy when it is cancelled"
    );
    // Past the statements still running on the worker thread.
    tokio::time::sleep(std::time::Duration::from_millis(3000)).await;

    for n in 0..8 {
        pool.execute_checked("INSERT INTO edits VALUES (?)", &[Value::Integer(n)], 1)
            .await
            .unwrap_or_else(|e| panic!("edit {n} failed on a pooled connection: {e}"));
    }
    let fresh = fixture.open().await;
    assert_eq!(
        count(&fresh, "SELECT count(*) FROM cancelled_tx").await,
        Value::Integer(0),
        "the cancelled script's uncommitted INSERT must be gone"
    );
    assert_eq!(
        count(&fresh, "SELECT count(*) FROM edits").await,
        Value::Integer(8)
    );
    fresh.close().await;
    pool.close().await;
}

/// A read with far more rows than the cap — two billion, generated as they
/// are sent (a set-returning function in the select list streams; in `FROM`
/// it would be materialized first). Reading the rest of it after the cap
/// would take minutes.
const PG_ENDLESS_READ: &str = "SELECT generate_series(1, 2000000000)";

// Multi-threaded, as the app's runtime is: a capped read with nothing
// open has the rest read in a background task before its connection closes,
// which on a single-threaded runtime holds the thread until it is done.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn postgres_script_ending_in_a_capped_read_does_not_wait_for_the_rest() {
    // Ending a run asks the connection whether the script left a transaction
    // open. Asked after a read the row cap cut short, that question waited
    // for the rest of the result to arrive and be thrown away — measured,
    // 1.3 s for 3M rows on localhost, against 30 ms before the run held its
    // connection. A one-statement SELECT is the commonest run there is.
    let Some(url) = pg_url().await else { return };
    let pool = DbPool::open_postgres(&url).await.unwrap();
    pool.execute("DROP TABLE IF EXISTS capped_tx")
        .await
        .unwrap();
    pool.execute("CREATE TABLE capped_tx (a integer)")
        .await
        .unwrap();
    common::open_several_connections(&pool, "SELECT pg_sleep(0.2)").await;
    let bound = std::time::Duration::from_secs(30);

    let (results, outcome) = tokio::time::timeout(bound, collect_script(&pool, PG_ENDLESS_READ))
        .await
        .expect("a capped read must not wait for the rest of its result");
    assert_eq!(outcome, Ok(ScriptDone::default()));
    assert!(results[0].truncated);
    // With nothing open, the rest is read in the background before the
    // connection closes — two billion rows here, so end that session rather
    // than let it run on behind the test.
    let terminated = count(
        &pool,
        &format!(
            "SELECT count(pg_terminate_backend(pid)) FROM pg_stat_activity \
             WHERE datname = current_database() AND pid <> pg_backend_pid() \
             AND query = '{PG_ENDLESS_READ}'"
        ),
    )
    .await;
    assert_eq!(
        terminated,
        Value::Integer(1),
        "the read is still being drained"
    );

    // The same read at the end of a script inside its own transaction: the
    // transaction is still rolled back, and said to be.
    let (results, outcome) = tokio::time::timeout(
        bound,
        collect_script(
            &pool,
            &format!("BEGIN; INSERT INTO capped_tx VALUES (1); {PG_ENDLESS_READ}"),
        ),
    )
    .await
    .expect("a capped read must not wait for the rest of its result");
    assert!(results[2].truncated);
    assert_eq!(
        outcome,
        Ok(ScriptDone {
            rolled_back: Some(OpenTransaction {
                schema_changes_kept: false
            }),
            connection_closed: false,
        })
    );
    assert_eq!(pg_open_transactions(&pool).await, Vec::<String>::new());
    assert_eq!(
        count(&pool, "SELECT count(*) FROM capped_tx").await,
        Value::Integer(0)
    );
    pool.execute("DROP TABLE capped_tx").await.unwrap();
    pool.close().await;
}

#[tokio::test]
async fn postgres_capped_row_returning_write_keeps_its_write() {
    // A data-modifying CTE that returns rows is a read by its first keyword,
    // and writes only as fast as its rows are read. Measured, closing the
    // connection after the row cap kept 0 rows of 3M; it must be read to the
    // end, and the write kept, before the run reports done.
    let Some(url) = pg_url().await else { return };
    let pool = DbPool::open_postgres(&url).await.unwrap();
    pool.execute("DROP TABLE IF EXISTS capped_write")
        .await
        .unwrap();
    pool.execute("CREATE TABLE capped_write (a integer)")
        .await
        .unwrap();

    let (results, outcome) = collect_script(
        &pool,
        "WITH w AS (INSERT INTO capped_write SELECT generate_series(1, 300000) RETURNING a) \
         SELECT a FROM w",
    )
    .await;
    assert_eq!(outcome, Ok(ScriptDone::default()));
    assert!(results[0].truncated);
    assert_eq!(
        count(&pool, "SELECT count(*) FROM capped_write").await,
        Value::Integer(300_000),
        "every row the statement wrote must be kept"
    );
    pool.execute("DROP TABLE capped_write").await.unwrap();
    pool.close().await;
}

// Multi-threaded, as the app's runtime is: a capped read with nothing
// open has the rest read in a background task before its connection closes,
// which on a single-threaded runtime holds the thread until it is done.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn postgres_capped_read_with_a_write_the_gate_cannot_see_keeps_it() {
    // `SELECT f()` for a function that writes looks like a plain read, so it
    // may be cut off at the cap — but never by closing the connection, which
    // aborted the statement. The rest is read in the background before the
    // connection closes, and the statement commits, as capped reads always did.
    let Some(url) = pg_url().await else { return };
    let pool = DbPool::open_postgres(&url).await.unwrap();
    pool.execute("DROP FUNCTION IF EXISTS capped_bump(integer)")
        .await
        .unwrap();
    pool.execute("DROP TABLE IF EXISTS capped_side")
        .await
        .unwrap();
    pool.execute("CREATE TABLE capped_side (a integer)")
        .await
        .unwrap();
    pool.execute(
        "CREATE FUNCTION capped_bump(i integer) RETURNS integer LANGUAGE sql \
         AS 'INSERT INTO capped_side VALUES (i) RETURNING a'",
    )
    .await
    .unwrap();

    let (results, outcome) = collect_script(
        &pool,
        "SELECT capped_bump(g) FROM generate_series(1, 20000) g",
    )
    .await;
    assert_eq!(outcome, Ok(ScriptDone::default()));
    assert!(results[0].truncated);
    let mut kept = Value::Null;
    for _ in 0..300 {
        kept = count(&pool, "SELECT count(*) FROM capped_side").await;
        if kept == Value::Integer(20_000) {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert_eq!(
        kept,
        Value::Integer(20_000),
        "the function's writes must be kept"
    );
    pool.execute("DROP FUNCTION capped_bump(integer)")
        .await
        .unwrap();
    pool.execute("DROP TABLE capped_side").await.unwrap();
    pool.close().await;
}
