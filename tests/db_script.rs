//! Integration tests for script execution: `DbPool::execute` affected-row
//! counts, atomic multi-statement runs (a mid-script failure rolls the whole
//! batch back), and Postgres error position enrichment.
//!
//! Postgres tests need a running server (Docker only, per CLAUDE.md) and are
//! skipped unless `HUBRO_PG_TEST_URL` is set — see tests/db_postgres.rs.

mod common;

use common::FixtureDb;
use hubro::db::{
    run_script, split_statements, DbError, DbPool, Rollback, StatementOutcome, StatementResult,
    Value,
};

async fn pg_url() -> Option<String> {
    common::pg_test_url().await
}

/// Runs a script (split from one SQL text) collecting per-statement results.
async fn collect_script(
    pool: &DbPool,
    sql: &str,
) -> (Vec<StatementResult>, Result<(), hubro::db::ScriptError>) {
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
