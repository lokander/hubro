//! CockroachDB verification. Cockroach reimplements the Postgres wire
//! protocol and SQL layer over its own storage rather than extending Postgres,
//! so this is the first test of how much hubro's Postgres backend assumes about
//! being talked to by actual PostgreSQL. These tests pin what is *not* stock.
//!
//! Needs a running server (Docker only, per CLAUDE.md) and is skipped unless
//! `HUBRO_CRDB_TEST_URL` is set, e.g.:
//!
//! ```sh
//! docker run -d --name hubro-crdb-test -p 26257:26257 \
//!   cockroachdb/cockroach:latest start-single-node --insecure
//! docker exec hubro-crdb-test ./cockroach sql --insecure -e 'CREATE DATABASE demo'
//! HUBRO_CRDB_TEST_URL='postgres://root@localhost:26257/demo?sslmode=disable' cargo test
//! ```
//!
//! `sslmode=disable` because `--insecure` serves no TLS at all; a secure
//! cluster is reached with an ordinary `sslmode=require` URL.
//!
//! ## What the verification found
//!
//! Two things needed fixing in the backend, both landed with these tests:
//!
//!  1. **Introspection failed outright.** Cockroach makes
//!     `information_schema.key_column_usage.ordinal_position` a 64-bit integer
//!     where Postgres makes it 32-bit, and the decode is exact by wire type —
//!     so one column's width took down the whole schema tree. The query now
//!     pins the width itself.
//!  2. **The schema tree was mostly Cockroach's.** `crdb_internal` and
//!     `pg_extension` hold 119 objects and are listed like user tables, and
//!     most of them cannot even be opened. Cockroach reports them as
//!     `table_type = 'SYSTEM VIEW'`, a value stock PostgreSQL never emits, so
//!     introspection now reads them off that: marked [`Internal::System`] for
//!     the sidebar to hide, and typed as views rather than tables.
//!
//! One is a known gap, left as it is on purpose:
//!
//!  3. **A script containing DDL does not roll back cleanly.** Cockroach's
//!     `autocommit_before_ddl` defaults to on, committing the open transaction
//!     *before* each DDL statement — so the schema change survives a failure,
//!     and so does every write staged ahead of it, while hubro reports the
//!     batch as rolled back. Only statements after the last DDL are still
//!     covered. Turning the setting off restores transactional DDL *and*
//!     breaks `ALTER TABLE`/`CREATE INDEX` against the schema-locked tables
//!     Cockroach creates by default, which is the more common operation, so
//!     the engine's default stands. See
//!     `cockroach_script_dml_rolls_back_but_its_ddl_does_not`, which pins the
//!     behaviour so the trade stays a decision rather than a surprise.
//!
//!     hubro no longer misreports it. This connection declares
//!     `transactional_ddl: false`, so a failing script that changed the schema
//!     says the rollback did not reach it instead of claiming no changes were
//!     applied. Behind a `Capabilities` flag rather than a check against this
//!     one engine, since YugabyteDB does the same thing for its own reasons.
//!     Note what it is *not*: `transactions` stays true, because DML in a
//!     script really does roll back here.
//!
//! And two are engine behaviour hubro should follow rather than fight:
//!
//!  4. **Every table has a key.** A table declared without a primary key gets
//!     an implicit `rowid` — a real, stored, NOT NULL column with a
//!     `unique_rowid()` default, visible in `information_schema`. So it
//!     arrives as an ordinary primary key and the table is editable, where the
//!     same table on Postgres would be read-only. It is stable in a way
//!     SQLite's rowid and Postgres's `ctid` are not, so this is sound, not a
//!     loophole (contrast `tests/db_timescale.rs`, where a `ctid` fallback
//!     would be actively wrong).
//!  5. **`serial` is not sequential.** It defaults to `unique_rowid()`, not
//!     `nextval(…)`, so values are large and non-consecutive. Nothing to fix —
//!     the column is still auto-assigned and inserts still omit it — but it is
//!     the assumption most likely to surprise.
//!
//! One more was found, and fixed, after the verification:
//!
//!  6. **A `real` is a 64-bit float.** `::real` rounds nothing — a column
//!     declared `real` holding `0.1` holds the float64 `0.1` — but the wire
//!     sends it as a 4-byte `float4`, so the grid reads `0.1f32`. Bound back
//!     widened (`0.10000000149011612::real`) that key matched no row: its long
//!     cells loaded as NULL, Save refused it, and a foreign key into it jumped
//!     to an empty page. Key positions now bind a `real` as the shortest
//!     spelling of its `f32` (`'0.1'::real`), which is the decimal the row was
//!     written with — see `cockroach_real_keys_are_re_addressed_by_their_shortest_spelling`.
//!     A `real` key written with more digits than an `f32` holds stays
//!     unreachable: the wire rounded them away before hubro saw the row. That
//!     test pins it too.
//!
//! ## Absent features, confirmed to fail sensibly rather than corrupt
//!
//! Declarative partitioning, `VACUUM`, range types, nested arrays, and
//! `COLLATE "C"` are all unsupported and surface as ordinary statement errors.
//! Cockroach also sends no error cursor position, so SQL editor errors carry
//! the message without the "(line L, column C)" suffix. The rest of the
//! Postgres suite passes against this container — point `HUBRO_PG_TEST_URL` at
//! it to re-run it there, minus the cases resting on those five features.

mod common;

use common::pg_family::{
    assert_pages_sorts_and_filters, assert_rows_edit_through_key, assert_schema_is_internal,
    assert_script_ddl_escapes_rollback, find, integer, DdlEscape, Expected, KeyedEdits,
    ReadingsKey, SensorFixture,
};

use hubro::db::Reading;
use hubro::db::{
    detect_row_identity, run_script, split_statements, DbPool, Filter, KeyTextPinned,
    OpenTransaction, PageRequest, PgFlavor, Restriction, Rollback, RowIdentity, ScriptDone,
    SortDir, StatementOutcome, TableKind, Value, MISSING_ROW,
};

fn test_url() -> Option<String> {
    common::engine_url("HUBRO_CRDB_TEST_URL")
}

/// A parent/child pair with a real primary key, a foreign key and a secondary
/// index. Suffixed per test so the tests in this binary can run concurrently
/// against one database without fighting over one fixture.
async fn fresh_fixture(pool: &DbPool, suffix: &str) -> (String, String) {
    SensorFixture {
        prefix: "",
        key: ReadingsKey::IdAndSensor,
        ddl_lock: None,
    }
    .create(pool, suffix)
    .await
}

#[tokio::test]
async fn cockroach_identifies_itself_as_its_own_engine() {
    let Some(url) = test_url() else { return };
    let pool = DbPool::open_postgres(&url).await.unwrap();

    // No introspection in this file depends on this answer, and that is
    // deliberate: every fix the CockroachDB verification landed reads a
    // catalog fact instead, so the backend behaves correctly here whether or
    // not it recognises the engine. Detection exists to *report* who answered,
    // and to declare what no catalog fact supplies: the capabilities, such as
    // this engine's `transactional_ddl: false`.
    // `stock_postgres_is_detected_as_stock_postgres` pins the other direction.
    assert_eq!(pool.pg_flavor(), Some(PgFlavor::CockroachDB));

    pool.close().await;
}

#[tokio::test]
async fn cockroach_introspects_tables_columns_keys_and_relationships() {
    let Some(url) = test_url() else { return };
    let pool = DbPool::open_postgres(&url).await.unwrap();
    let (readings, sensors) = fresh_fixture(&pool, "intro").await;

    // The regression guard for the bug that made this whole call fail: a
    // 64-bit `ordinal_position` in key_column_usage. Nothing below is
    // reachable if introspection errors, which is exactly what it did.
    let tables = pool.introspect().await.unwrap();
    let table = find(&tables, &readings);

    assert_eq!(table.kind, TableKind::Table);
    assert_eq!(table.internal, None);
    let columns: Vec<&str> = table.columns.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(columns, ["id", "sensor_id", "temperature", "note"]);

    // The composite key, in key order — the part the width bug destroyed.
    let pk: Vec<&str> = table
        .primary_key()
        .iter()
        .map(|c| c.name.as_str())
        .collect();
    assert_eq!(pk, ["id", "sensor_id"]);

    let fk = table.foreign_keys.first().expect("fk to the sensors table");
    assert_eq!(fk.columns, ["sensor_id"]);
    assert_eq!(fk.referenced_table, sensors);
    assert_eq!(fk.referenced_columns, [Some("id".to_string())]);

    assert!(
        table
            .indexes
            .iter()
            .any(|i| i.columns == ["temperature"] && !i.unique),
        "expected the secondary index, got {:?}",
        table.indexes
    );

    pool.close().await;
}

#[tokio::test]
async fn cockroach_reserved_catalog_schemas_are_marked_as_the_engines_own() {
    let Some(url) = test_url() else { return };
    let pool = DbPool::open_postgres(&url).await.unwrap();
    // A table of this test's own to contrast against, so the assertion below
    // rests on something this test created rather than on whatever a sibling
    // test happened to leave behind.
    fresh_fixture(&pool, "reserved").await;

    let tables = pool.introspect().await.unwrap();

    // Cockroach lists `crdb_internal` and `pg_extension` in
    // information_schema.tables exactly as it lists user tables — over a
    // hundred objects, most of which refuse to be read at all. Postgres keeps
    // the equivalent in the two schemas introspection already excludes, so
    // nothing else here catches these: they are not extension members (the
    // pg_depend path finds nothing on Cockroach) and not partitions.
    //
    // Selected by schema *here* even though the backend classifies them by
    // `table_type`, which is the point: the test names the objects
    // independently of the rule under test, so a rule that quietly stopped
    // matching would fail this rather than select nothing and pass.
    let reserved = assert_schema_is_internal(
        &tables,
        |schema| matches!(schema, "crdb_internal" | "pg_extension"),
        50,
    );
    for table in &reserved {
        // A `SYSTEM VIEW` is a view: nothing about being the engine's own
        // catalog makes its rows addressable, and reading it as a table would
        // offer editing on objects that mostly cannot even be read.
        assert_eq!(table.kind, TableKind::View, "{table:?}");
        assert_eq!(
            detect_row_identity(table, pool.dialect()),
            None,
            "{table:?}"
        );
    }

    pool.close().await;
}

#[tokio::test]
async fn cockroach_pages_sorts_and_filters() {
    let Some(url) = test_url() else { return };
    let pool = DbPool::open_postgres(&url).await.unwrap();
    let (readings, _) = fresh_fixture(&pool, "page").await;

    let request = PageRequest {
        schema: Some("public".into()),
        limit: 3,
        sort: Some(("id".into(), SortDir::Desc)),
        filter: Some(Filter::equals("sensor_id", "2")),
        ..common::page(readings.clone())
    };
    let (page, next) = assert_pages_sorts_and_filters(&pool, &request, Expected::Exactly(8)).await;
    let ids: Vec<i64> = page.rows.iter().map(|row| integer(&row[0])).collect();
    assert_eq!(ids, [8, 7, 6], "rows should come back highest id first");
    assert_eq!(integer(&next.rows[0][0]), 5);

    pool.close().await;
}

#[tokio::test]
async fn cockroach_rows_edit_through_the_composite_key() {
    let Some(url) = test_url() else { return };
    let pool = DbPool::open_postgres(&url).await.unwrap();
    let (readings, _) = fresh_fixture(&pool, "edit").await;

    let tables = pool.introspect().await.unwrap();
    assert_rows_edit_through_key(
        &pool,
        find(&tables, &readings),
        &["id", "sensor_id"],
        KeyedEdits {
            update: vec![Value::Integer(1), Value::Integer(1)],
            insert: (
                &["id", "sensor_id", "temperature"],
                vec![Value::Integer(99), Value::Integer(1), Value::Real(9.5)],
            ),
            delete: vec![Value::Integer(99), Value::Integer(1)],
        },
    )
    .await;

    pool.close().await;
}

#[tokio::test]
async fn cockroach_table_without_a_declared_key_is_still_editable_through_its_rowid() {
    let Some(url) = test_url() else { return };
    let pool = DbPool::open_postgres(&url).await.unwrap();

    // The same table on Postgres is read-only: no primary key, no unique
    // index, no addressable row. Cockroach gives it an implicit `rowid` and —
    // unlike SQLite's rowid or Postgres's ctid — that is a real stored column
    // with a unique default, not a physical locator that a VACUUM or a
    // rewrite can reassign. So it arrives as an ordinary primary key and the
    // table is genuinely editable.
    pool.query_unbounded("DROP TABLE IF EXISTS crdb_nokey CASCADE")
        .await
        .unwrap();
    pool.query_unbounded("CREATE TABLE crdb_nokey (a int, b text)")
        .await
        .unwrap();
    pool.query_unbounded("INSERT INTO crdb_nokey (a, b) VALUES (1, 'one'), (2, 'two')")
        .await
        .unwrap();

    let tables = pool.introspect().await.unwrap();
    let table = find(&tables, "crdb_nokey");

    // The implicit column is visible in the catalog, so it is visible here —
    // introspection reports what the engine says rather than what was typed.
    let columns: Vec<&str> = table.columns.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(columns, ["a", "b", "rowid"]);
    assert_eq!(
        detect_row_identity(table, pool.dialect()),
        Some(RowIdentity::PrimaryKey {
            columns: vec!["rowid".into()]
        })
    );
    assert!(pool.backend_access(table).can_mutate());

    // The database supplies it, so an insert must not be asked to.
    let rowid = table.columns.iter().find(|c| c.name == "rowid").unwrap();
    assert!(!rowid.nullable);
    assert!(
        rowid.is_auto_assigned(),
        "rowid has a unique_rowid() default and must never be a required insert column"
    );

    pool.query_unbounded("DROP TABLE crdb_nokey CASCADE")
        .await
        .unwrap();
    pool.close().await;
}

#[tokio::test]
async fn cockroach_view_is_readable_but_never_editable() {
    let Some(url) = test_url() else { return };
    let pool = DbPool::open_postgres(&url).await.unwrap();
    let (readings, _) = fresh_fixture(&pool, "view").await;
    let view = format!("{readings}_warm");

    pool.query_unbounded(&format!("DROP VIEW IF EXISTS {view}"))
        .await
        .unwrap();
    pool.query_unbounded(&format!(
        "CREATE VIEW {view} AS SELECT id, temperature FROM {readings} WHERE temperature > 3"
    ))
    .await
    .unwrap();

    let tables = pool.introspect().await.unwrap();
    let meta = find(&tables, &view);
    assert_eq!(meta.kind, TableKind::View);
    assert_eq!(detect_row_identity(meta, pool.dialect()), None);

    let access = pool.backend_access(meta);
    assert!(!access.can_mutate());
    assert_eq!(access.restriction, Some(Restriction::View));

    let page = pool
        .fetch_page(
            Reading::UNMARKED,
            &PageRequest {
                schema: Some("public".into()),
                limit: 10,
                sort: Some(("id".into(), SortDir::Asc)),
                ..common::page(view.clone())
            },
        )
        .await
        .unwrap();
    assert!(!page.rows.is_empty(), "the view should have rows");

    pool.query_unbounded(&format!("DROP VIEW {view}"))
        .await
        .unwrap();
    pool.close().await;
}

#[tokio::test]
async fn cockroach_script_dml_rolls_back_but_its_ddl_does_not() {
    let Some(url) = test_url() else { return };
    let pool = DbPool::open_postgres(&url).await.unwrap();

    // The engine behaviour, pinned so it stays deliberate. Cockroach's
    // `autocommit_before_ddl` defaults to on: it commits the open transaction
    // before each DDL statement, announcing it only as a NOTICE. A failing
    // script therefore rolls back its *writes* but leaves its schema changes
    // standing.
    //
    // hubro no longer claims otherwise: this connection declares
    // `transactional_ddl: false`, so a script containing DDL reports
    // `Rollback::ExceptSchemaChanges` rather than a flat "rolled back".
    // What survives below is unchanged — the fix was to the claim,
    // not to the engine.
    //
    // Setting `autocommit_before_ddl = false` fixes exactly this and was
    // tried. It also makes DDL against a schema-locked table fail, and
    // Cockroach schema-locks tables on creation by default — so it breaks
    // `ALTER TABLE` and `CREATE INDEX` against any table hubro did not itself
    // create, which is a worse and far more common failure. The engine's
    // default therefore stands; if this test ever starts failing because the
    // CREATE TABLE *did* roll back, that trade has been revisited (or
    // Cockroach changed its default) and the header above needs updating.
    assert_script_ddl_escapes_rollback(
        &pool,
        DdlEscape {
            prefix: "crdb_",
            ddl_lock: None,
            // The auto-commit fires *before* the DDL, so it also commits
            // everything staged ahead of it: the INSERT is not itself a
            // schema change and still survives. That is the sharp edge — the
            // escape isn't limited to the DDL statement, it takes every write
            // before it along.
            rows_left: (
                1,
                "known gap: the commit before the DDL takes the preceding INSERT with it",
            ),
            table_left: "known gap: Cockroach auto-commits DDL out of the transaction",
        },
    )
    .await;
    pool.close().await;
}

#[tokio::test]
async fn cockroach_reports_a_full_rollback_when_the_script_never_reached_its_ddl() {
    let Some(url) = test_url() else { return };
    let pool = DbPool::open_postgres(&url).await.unwrap();

    // The other side of the previous test, and the one that is easy to get
    // wrong: whether a schema change escaped the rollback depends on whether it
    // *ran*, not on whether the script contains one. Here the failure comes
    // first, so the CREATE TABLE never reaches the server and the transaction
    // rolls back completely — a warning about surviving schema changes would
    // describe a table that does not exist, which is the same false claim as
    // calling a committed schema change rolled back, pointing the other way.
    pool.query_unbounded("DROP TABLE IF EXISTS crdb_unreached_ddl CASCADE")
        .await
        .unwrap();
    pool.query_unbounded("DROP TABLE IF EXISTS crdb_unreached_dml CASCADE")
        .await
        .unwrap();
    pool.query_unbounded("CREATE TABLE crdb_unreached_dml (id int PRIMARY KEY)")
        .await
        .unwrap();

    let sql = "INSERT INTO crdb_unreached_dml VALUES (1); \
               SELECT * FROM missing_relation; \
               CREATE TABLE crdb_unreached_ddl (id int PRIMARY KEY)";
    let statements = split_statements(sql, pool.dialect());
    let error = run_script(&pool, pool.backend_capabilities(), &statements, |_| {})
        .await
        .expect_err("the second statement names no relation");
    assert_eq!(error.statement_index, 1);
    assert_eq!(
        error.rollback,
        Rollback::Full,
        "the DDL never ran, so the rollback really did cover everything"
    );

    // ...and the database agrees on both halves.
    let rows = pool
        .query_unbounded("SELECT count(*) FROM crdb_unreached_dml")
        .await
        .unwrap();
    assert_eq!(
        integer(&rows.rows[0][0]),
        0,
        "the INSERT rolled back — no DDL ran to commit it early"
    );
    let survivors = pool
        .query_unbounded(
            "SELECT count(*) FROM information_schema.tables \
             WHERE table_name = 'crdb_unreached_ddl'",
        )
        .await
        .unwrap();
    assert_eq!(
        integer(&survivors.rows[0][0]),
        0,
        "the table was never created"
    );

    pool.query_unbounded("DROP TABLE crdb_unreached_dml CASCADE")
        .await
        .unwrap();
    pool.close().await;
}

#[tokio::test]
async fn cockroach_counts_a_failing_ddl_as_having_escaped_the_rollback() {
    let Some(url) = test_url() else { return };
    let pool = DbPool::open_postgres(&url).await.unwrap();

    // The boundary between this test and the one above, which is a real
    // decision rather than an off-by-one: the statements a rollback claim is
    // resolved over *include* the failing one. A DDL that fails has still done
    // its damage here, because `autocommit_before_ddl` commits the open
    // transaction before the statement executes — so the write staged ahead of
    // it survives even though the schema change never happened.
    //
    // Resolving over the statements strictly before the failure would report
    // "no changes were applied" for exactly this shape, which is the old bug
    // of reporting committed DDL as rolled back, returning by a narrower route.
    pool.query_unbounded("DROP TABLE IF EXISTS crdb_boundary_dup CASCADE")
        .await
        .unwrap();
    pool.query_unbounded("DROP TABLE IF EXISTS crdb_boundary_dml CASCADE")
        .await
        .unwrap();
    pool.query_unbounded("CREATE TABLE crdb_boundary_dml (id int PRIMARY KEY)")
        .await
        .unwrap();
    // Already present, so the script's CREATE TABLE is the statement that fails.
    pool.query_unbounded("CREATE TABLE crdb_boundary_dup (id int PRIMARY KEY)")
        .await
        .unwrap();

    let sql = "INSERT INTO crdb_boundary_dml VALUES (1); \
               CREATE TABLE crdb_boundary_dup (id int PRIMARY KEY)";
    let statements = split_statements(sql, pool.dialect());
    let error = run_script(&pool, pool.backend_capabilities(), &statements, |_| {})
        .await
        .expect_err("the table already exists");
    assert_eq!(error.statement_index, 1, "the DDL is the failing statement");
    assert_eq!(
        error.rollback,
        Rollback::ExceptSchemaChanges,
        "a failing DDL still commits what was staged before it"
    );

    // The database agrees: the INSERT survived a rollback that claimed nothing
    // about it either way.
    let rows = pool
        .query_unbounded("SELECT count(*) FROM crdb_boundary_dml")
        .await
        .unwrap();
    assert_eq!(
        integer(&rows.rows[0][0]),
        1,
        "the autocommit fired before the failing DDL and took the INSERT with it"
    );

    pool.query_unbounded("DROP TABLE crdb_boundary_dup CASCADE")
        .await
        .unwrap();
    pool.query_unbounded("DROP TABLE crdb_boundary_dml CASCADE")
        .await
        .unwrap();
    pool.close().await;
}

#[tokio::test]
async fn cockroach_serial_is_unique_but_not_sequential() {
    let Some(url) = test_url() else { return };
    let pool = DbPool::open_postgres(&url).await.unwrap();

    // Recorded because it is the assumption most likely to be made silently:
    // `serial` here defaults to `unique_rowid()`, not to a sequence, so the
    // values are large and non-consecutive. Anything that expects the first
    // inserted row to have id 1 — a test fixture, a filter typed by habit —
    // finds nothing. hubro needs no change for it: what the backend actually
    // depends on is that the column is auto-assigned, which it is.
    pool.query_unbounded("DROP TABLE IF EXISTS crdb_serial CASCADE")
        .await
        .unwrap();
    pool.query_unbounded("CREATE TABLE crdb_serial (id serial PRIMARY KEY, name text)")
        .await
        .unwrap();
    pool.query_unbounded("INSERT INTO crdb_serial (name) VALUES ('a'), ('b')")
        .await
        .unwrap();

    let tables = pool.introspect().await.unwrap();
    let table = find(&tables, "crdb_serial");
    let id = table.columns.iter().find(|c| c.name == "id").unwrap();
    assert!(
        id.is_auto_assigned(),
        "the property hubro relies on: the database supplies the value"
    );
    assert!(
        id.default.as_deref().unwrap().contains("unique_rowid"),
        "expected a unique_rowid() default, got {:?}",
        id.default
    );

    let rows = pool
        .query_unbounded("SELECT id FROM crdb_serial ORDER BY id")
        .await
        .unwrap();
    let ids: Vec<i64> = rows.rows.iter().map(|row| integer(&row[0])).collect();
    assert_eq!(ids.len(), 2);
    assert!(
        ids[0] > 1_000_000,
        "serial values are unique_rowid()-shaped, not 1 and 2: {ids:?}"
    );

    pool.query_unbounded("DROP TABLE crdb_serial CASCADE")
        .await
        .unwrap();
    pool.close().await;
}

#[tokio::test]
async fn cockroach_keeps_no_statistics_this_can_read_so_the_pane_stays_empty() {
    let Some(url) = test_url() else { return };
    let pool = DbPool::open_postgres(&url).await.unwrap();
    let (readings, _) = fresh_fixture(&pool, "stats").await;
    let tables = pool.introspect().await.unwrap();
    let meta = find(&tables, &readings);

    // Cockroach has `pg_class.reltuples` and leaves it NULL for every
    // relation, and ships no `pg_total_relation_size` at all — its own
    // `crdb_internal.table_row_statistics` is refused outright ("Access to
    // crdb_internal and system is restricted"). So the honest answer is that
    // there is no answer.
    //
    // What this pins is that the absence stays an absence: the shape probe
    // must return `Ok` with nothing in it rather than erroring the pane, and
    // nothing may substitute a zero for the missing numbers.
    let stats = pool.fetch_table_stats(meta).await.unwrap();
    assert!(
        stats.is_empty(),
        "CockroachDB keeps no readable statistics, so nothing may be claimed: {stats:?}"
    );
    assert_eq!(stats.rows, None);
    assert_eq!(stats.bytes, None);

    // The exact count is the number this engine *can* give, and the reason
    // the action exists separately from the estimate.
    assert_eq!(
        pool.count_table_rows(Reading::UNMARKED, meta)
            .await
            .unwrap(),
        16
    );

    pool.query_unbounded(&format!("DROP TABLE IF EXISTS {readings} CASCADE"))
        .await
        .unwrap();
    pool.close().await;
}

#[tokio::test]
async fn cockroach_real_keys_are_re_addressed_by_their_shortest_spelling() {
    // Cockroach keeps a `real` as a 64-bit float — `::real` rounds nothing —
    // while the wire sends it as a 4-byte `float4`. A key read back as
    // `0.1f32` and bound widened (`0.10000000149011612::real`) matched no row,
    // so the cell expanded to NULL, Save refused the row and a foreign key
    // into it jumped to an empty page. Bound as the f32's shortest spelling
    // (`'0.1'::real`) it is the decimal the row was written with.
    let Some(url) = test_url() else { return };
    let pool = DbPool::open_postgres(&url).await.unwrap();
    let body = |n: usize| format!("'{n}:' || repeat('x', 5000)");
    let literals = [
        "0.1",
        "1e20",
        "3.4028235e38",
        "1e-45",
        "'NaN'",
        "'-Infinity'",
    ];
    let keys: Vec<String> = literals
        .iter()
        .enumerate()
        .map(|(n, lit)| format!("({lit}, {}, 't')", body(n)))
        .collect();
    let refs: Vec<String> = literals
        .iter()
        .enumerate()
        .map(|(n, lit)| format!("({lit}, '{n}:')"))
        .collect();
    for sql in [
        "DROP TABLE IF EXISTS real_refs, real_keys, real_precise".to_string(),
        "CREATE TABLE real_keys (k real PRIMARY KEY, body text, tag text)".to_string(),
        format!("INSERT INTO real_keys VALUES {}", keys.join(", ")),
        "CREATE TABLE real_refs (ref real REFERENCES real_keys (k), expect text)".to_string(),
        format!("INSERT INTO real_refs VALUES {}", refs.join(", ")),
        // More digits than an f32 holds, which this engine's `real` keeps.
        "CREATE TABLE real_precise (k real PRIMARY KEY, body text, tag text)".to_string(),
        format!(
            "INSERT INTO real_precise VALUES (0.123456789012, {}, 't')",
            body(0)
        ),
    ] {
        pool.query_unbounded(&sql)
            .await
            .unwrap_or_else(|e| panic!("{sql}: {e}"));
    }
    let tables = pool.introspect().await.unwrap();
    let (keyed, refs) = (find(&tables, "real_keys"), find(&tables, "real_refs"));
    let mut failures = common::row_addressing_failures(&pool, keyed).await;
    failures.extend(common::fk_jump_failures(&pool, refs, keyed).await);
    assert!(failures.is_empty(), "{}", failures.join("\n"));

    // The known residual, pinned so it stays one: the wire rounded the extra
    // digits away before hubro saw the row, so no bind can find it again —
    // its long cell does not load and its edit is refused (never applied to
    // another row). If this starts passing, the engine changed and the
    // comment on `key_bind_value` is out of date.
    let precise = common::row_addressing_failures(&pool, find(&tables, "real_precise")).await;
    assert_eq!(precise.len(), 2, "{}", precise.join("\n"));
    assert!(
        precise[0].ends_with(&format!("fetch_cell failed: query failed: {MISSING_ROW}"))
            && precise[1].contains("the write did not address its row"),
        "{}",
        precise.join("\n")
    );

    pool.query_unbounded("DROP TABLE real_refs, real_keys, real_precise")
        .await
        .unwrap();
    pool.close().await;
}

/// Cockroach sends `DECIMAL`, dates and times in Postgres's binary encoding,
/// so the values wider than the Rust types sqlx decodes those into broke here
/// as on PostgreSQL: a `1e30` panicked the page fetch (and with it the app),
/// NaN and more than 28 digits read as a marker no key can be bound from, and
/// `24:00:00` read as `00:00:00` — the key of a different row. They now read
/// as the server's own values and address their rows.
///
/// Two differences from PostgreSQL, both measured. Cockroach reads an
/// astronomical year back (`0000-01-01` is 1 BC to it), so the BC dates that
/// PostgreSQL refused as keys were only *shown* wrong here — the display
/// assertions are what pin them. And it prints a decimal in exponent form
/// (`1E+30`, `1E-31`) where the grid shows every digit: the same value, which
/// the round trip is what proves.
#[tokio::test]
async fn cockroach_values_past_the_rust_types_read_and_address_their_rows() {
    let Some(url) = test_url() else { return };
    let pool = DbPool::open_postgres(&url).await.unwrap();
    let shown = [
        ("1e30::decimal", "1000000000000000000000000000000"),
        (
            "0.0000000000000000000000000000001::decimal",
            "0.0000000000000000000000000000001",
        ),
        ("'NaN'::decimal", "NaN"),
        ("1.5::decimal(10, 2)", "1.50"),
        (
            "ARRAY[1e30, 'NaN', NULL]::decimal[]",
            "{1000000000000000000000000000000,NaN,NULL}",
        ),
        ("'0001-01-01 BC'::date", "0001-01-01 BC"),
        ("'10000-01-01'::date", "10000-01-01"),
        (
            "'0044-03-15 12:00:00.5 BC'::timestamp",
            "0044-03-15 12:00:00.5 BC",
        ),
        (
            "'0001-01-01 00:00:00+00 BC'::timestamptz",
            "0001-01-01 00:00:00+00:00 BC",
        ),
        ("'24:00:00'::time", "24:00:00"),
        ("'24:00:00+02'::timetz", "24:00:00+02:00"),
    ];
    let select: Vec<&str> = shown.iter().map(|(expr, _)| *expr).collect();
    let result = pool
        .query_unbounded(&format!("SELECT {}", select.join(", ")))
        .await
        .unwrap();
    for (n, (expr, text)) in shown.iter().enumerate() {
        assert_eq!(result.rows[0][n], Value::Text((*text).into()), "{expr}");
    }

    let keys: &[(&str, &str, &[&str])] = &[
        (
            "decimal",
            "decimal",
            &[
                "1e30",
                "123456789012345678901234567890.123456789",
                "0.0000000000000000000000000000001",
                "'NaN'",
            ],
        ),
        (
            "date",
            "date",
            &["'0001-01-01 BC'", "'10000-01-01'", "'infinity'"],
        ),
        ("ts", "timestamp", &["'0044-03-15 12:00:00.5 BC'"]),
        ("tstz", "timestamptz", &["'0001-01-01 00:00:00+00 BC'"]),
        ("time", "time", &["'24:00:00'", "'00:00:00'"]),
        (
            "timetz",
            "timetz",
            &["'24:00:00-03:30'", "'00:00:00-03:30'"],
        ),
    ];
    let mut failures = Vec::new();
    for (slug, type_name, literals) in keys {
        let table = format!("wide_key_{slug}");
        pool.query_unbounded(&format!("DROP TABLE IF EXISTS {table}"))
            .await
            .unwrap();
        pool.query_unbounded(&format!(
            "CREATE TABLE {table} (k {type_name} PRIMARY KEY, body text, tag text)"
        ))
        .await
        .unwrap();
        let rows: Vec<String> = literals
            .iter()
            .enumerate()
            .map(|(n, lit)| format!("({lit}, '{n}:' || repeat('x', 5000), 't')"))
            .collect();
        pool.query_unbounded(&format!("INSERT INTO {table} VALUES {}", rows.join(", ")))
            .await
            .unwrap_or_else(|e| panic!("{slug}: {e}"));
        let tables = pool.introspect().await.unwrap();
        failures.extend(common::row_addressing_failures(&pool, find(&tables, &table)).await);
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
    pool.close().await;
}

/// Each pooled connection's transaction status as CockroachDB reports it —
/// `NoTxn` outside one — from four reads at once, slow enough that each
/// needs a connection of its own. A connection inside an aborted transaction
/// fails the read instead.
async fn crdb_open_transactions(pool: &DbPool) -> Vec<String> {
    let reads: Vec<_> = (0..4)
        .map(|_| pool.query_unbounded("SELECT *, pg_sleep(0.2) FROM [SHOW TRANSACTION STATUS]"))
        .collect();
    futures_util::future::join_all(reads)
        .await
        .into_iter()
        .filter_map(|read| match read {
            Ok(result) if result.rows[0][0] == Value::Text("NoTxn".into()) => None,
            Ok(result) => Some(format!("{:?}", result.rows[0][0])),
            Err(e) => Some(e.to_string()),
        })
        .collect()
}

#[tokio::test]
async fn cockroach_script_keeps_its_own_transaction_on_one_session() {
    // The sequential path once took a pooled connection per statement, so
    // `BEGIN` and the statements after it reported different
    // `pg_backend_pid()`s, `ROLLBACK` undid nothing, and a transaction left
    // open swallowed later statements while a reader on another connection
    // waited on its locks.
    let Some(url) = test_url() else { return };
    let pool = DbPool::open_postgres(&url).await.unwrap();
    pool.execute("DROP TABLE IF EXISTS own_tx").await.unwrap();
    pool.execute("CREATE TABLE own_tx (a INT8)").await.unwrap();
    let opens: Vec<_> = (0..4)
        .map(|_| pool.query_unbounded("SELECT pg_sleep(0.2)"))
        .collect();
    for open in futures_util::future::join_all(opens).await {
        open.unwrap();
    }
    let caps = pool.backend_capabilities();
    let run = |sql: &str| {
        let statements = split_statements(sql, pool.dialect());
        let pool = &pool;
        async move {
            let mut rows = Vec::new();
            let outcome = run_script(pool, caps, &statements, |r| {
                if let StatementOutcome::Rows(result) = r.outcome {
                    rows.push(result.rows[0][0].clone());
                }
            })
            .await;
            (rows, outcome)
        }
    };

    let (pids, outcome) = run(
        "SELECT pg_backend_pid(); BEGIN; SELECT pg_backend_pid(); INSERT INTO own_tx VALUES (1); \
         SELECT pg_backend_pid(); ROLLBACK; SELECT pg_backend_pid()",
    )
    .await;
    assert_eq!(outcome, Ok(ScriptDone::default()));
    assert_eq!(pids.len(), 4);
    assert!(
        pids.iter().all(|pid| *pid == pids[0]),
        "every statement must run in one session, got {pids:?}"
    );
    let rows = pool
        .query_unbounded("SELECT count(*) FROM own_tx")
        .await
        .unwrap();
    assert_eq!(
        rows.rows[0][0],
        Value::Integer(0),
        "ROLLBACK must undo the INSERT"
    );

    let (_, outcome) = run("BEGIN; INSERT INTO own_tx VALUES (2)").await;
    assert_eq!(
        outcome,
        Ok(ScriptDone {
            rolled_back: Some(OpenTransaction {
                schema_changes_kept: false
            }),
            connection_closed: false,
        })
    );
    assert_eq!(crdb_open_transactions(&pool).await, Vec::<String>::new());

    let (_, outcome) =
        run("BEGIN; INSERT INTO own_tx VALUES (3); SELECT * FROM no_such_table; COMMIT").await;
    let err = outcome.unwrap_err();
    assert_eq!(err.statement_index, 2);
    assert_eq!(
        err.rollback,
        Rollback::OpenTransaction(OpenTransaction {
            schema_changes_kept: false
        })
    );
    assert_eq!(crdb_open_transactions(&pool).await, Vec::<String>::new());
    let rows = pool
        .query_unbounded("SELECT count(*) FROM own_tx")
        .await
        .unwrap();
    assert_eq!(rows.rows[0][0], Value::Integer(0));

    pool.execute("DROP TABLE own_tx").await.unwrap();
    pool.close().await;
}

#[tokio::test]
async fn cockroach_a_row_returning_statement_that_commits_is_not_reported_rolled_back() {
    // `SHOW COMMIT TIMESTAMP` returns a row and commits the open transaction.
    // Asked before it whether a transaction was open, the run said yes and
    // reported the script's writes as rolled back while they were committed.
    let Some(url) = test_url() else { return };
    let pool = DbPool::open_postgres(&url).await.unwrap();
    pool.execute("DROP TABLE IF EXISTS show_commit")
        .await
        .unwrap();
    pool.execute("CREATE TABLE show_commit (a INT8)")
        .await
        .unwrap();
    let statements = split_statements(
        "BEGIN; INSERT INTO show_commit VALUES (1); SHOW COMMIT TIMESTAMP",
        pool.dialect(),
    );
    let outcome = run_script(&pool, pool.backend_capabilities(), &statements, |_| {}).await;
    assert_eq!(outcome, Ok(ScriptDone::default()));
    let rows = pool
        .query_unbounded("SELECT count(*) FROM show_commit")
        .await
        .unwrap();
    assert_eq!(
        rows.rows[0][0],
        Value::Integer(1),
        "the INSERT was committed"
    );
    pool.execute("DROP TABLE show_commit").await.unwrap();
    pool.close().await;
}

#[tokio::test]
async fn cockroach_a_script_whose_show_commit_timestamp_commits_is_not_wrapped() {
    // Wrapped in hubro's transaction, the SHOW committed it and the failure
    // after it was reported as a full rollback with rows in the table.
    let Some(url) = test_url() else { return };
    let pool = DbPool::open_postgres(&url).await.unwrap();
    pool.execute("DROP TABLE IF EXISTS show_commit_wrapped")
        .await
        .unwrap();
    pool.execute("CREATE TABLE show_commit_wrapped (a INT8)")
        .await
        .unwrap();
    let statements = split_statements(
        "INSERT INTO show_commit_wrapped VALUES (1); SHOW COMMIT TIMESTAMP; \
         INSERT INTO show_commit_wrapped VALUES (2); SELECT * FROM no_such_table",
        pool.dialect(),
    );
    let outcome = run_script(&pool, pool.backend_capabilities(), &statements, |_| {}).await;
    let rows = pool
        .query_unbounded("SELECT a FROM show_commit_wrapped ORDER BY a")
        .await
        .unwrap();
    let err = outcome.unwrap_err();
    assert_eq!(
        (err.statement_index, err.rollback),
        (3, Rollback::None),
        "{}",
        err.error
    );
    assert_eq!(
        rows.rows,
        [[Value::Integer(1)], [Value::Integer(2)]],
        "run in autocommit, as reported"
    );
    pool.execute("DROP TABLE show_commit_wrapped")
        .await
        .unwrap();
    pool.close().await;
}

/// A `real` once read as the `f32` widened to `f64` — `0.1` showed as
/// `0.10000000149011612` in the grid, the export and the clipboard. It reads
/// as its shortest decimal now, and that decimal must still store the same
/// value when written back.
#[tokio::test]
async fn cockroach_a_real_reads_as_its_shortest_decimal_and_writes_back_unchanged() {
    let Some(url) = test_url() else { return };
    let pool = DbPool::open_postgres(&url).await.unwrap();
    let failures = common::real_round_trip_failures(&pool, "real_trip", async |name| {
        let tables = pool.introspect().await.unwrap();
        tables
            .into_iter()
            .find(|t| t.name == name)
            .unwrap_or_else(|| panic!("{name} missing from introspection"))
    })
    .await;
    assert!(failures.is_empty(), "{}", failures.join("\n"));
    pool.close().await;
}

/// The grid shows a NaN or an infinity, so the editor must take one back:
/// an unchanged NaN cell re-saves, and a typed `Infinity` stores.
#[tokio::test]
async fn cockroach_nan_and_infinity_go_through_the_editor_and_back() {
    let Some(url) = test_url() else { return };
    let pool = DbPool::open_postgres(&url).await.unwrap();
    let failures = common::non_finite_edit_failures(&pool, "non_finite_edits", async |name| {
        let tables = pool.introspect().await.unwrap();
        tables
            .into_iter()
            .find(|t| t.name == name)
            .unwrap_or_else(|| panic!("{name} missing from introspection"))
    })
    .await;
    assert!(failures.is_empty(), "{}", failures.join("\n"));
    pool.close().await;
}

#[tokio::test]
async fn cockroach_pins_interval_style_and_has_no_monetary_setting_to_pin() {
    // Whether a pin reaches the server is asked of it: CockroachDB takes
    // `IntervalStyle` at login, and has only `C.UTF-8` for `lc_monetary` (and
    // no `money` type), so that probe fails and nothing is pinned for it —
    // which refuses only `money` keys, which cannot exist here.
    let Some(url) = test_url() else { return };
    let pool = DbPool::open_postgres(&url).await.unwrap();
    assert_eq!(
        pool.key_rules().pinned,
        KeyTextPinned {
            interval_style: true,
            lc_monetary: false,
        }
    );
    pool.close().await;
}
