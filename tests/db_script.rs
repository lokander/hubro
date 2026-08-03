//! Integration tests for script execution: `DbPool::execute` affected-row
//! counts, atomic multi-statement runs (a mid-script failure rolls the whole
//! batch back — FRE-38), and Postgres error position enrichment.
//!
//! Postgres tests need a running server (Docker only, per CLAUDE.md) and are
//! skipped unless `DATAVIEW_PG_TEST_URL` is set — see tests/db_postgres.rs.

mod common;

use common::FixtureDb;
use dataview::db::{
    run_script, split_statements, DbError, DbPool, StatementOutcome, StatementResult, Value,
};

fn pg_url() -> Option<String> {
    match std::env::var("DATAVIEW_PG_TEST_URL") {
        Ok(url) => Some(url),
        Err(_) => {
            eprintln!("skipping postgres test: DATAVIEW_PG_TEST_URL not set");
            None
        }
    }
}

/// Runs a script (split from one SQL text) collecting per-statement results.
async fn collect_script(
    pool: &DbPool,
    sql: &str,
) -> (Vec<StatementResult>, Result<(), dataview::db::ScriptError>) {
    let statements = split_statements(sql, pool.dialect());
    let mut results = Vec::new();
    let outcome = run_script(pool, &statements, |r| results.push(r)).await;
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
    assert!(err.rolled_back, "a multi-statement script wraps atomically");

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
    assert!(
        !err.rolled_back,
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
    let Some(url) = pg_url() else { return };
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
    let Some(url) = pg_url() else { return };
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
    let Some(url) = pg_url() else { return };
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
    assert!(err.rolled_back, "a multi-statement script wraps atomically");

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
    let Some(url) = pg_url() else { return };
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
    let Some(url) = pg_url() else { return };
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
    assert!(
        !err.rolled_back,
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
