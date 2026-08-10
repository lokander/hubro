//! CockroachDB verification (FRE-90). Cockroach reimplements the Postgres wire
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
//!     hubro no longer misreports it (FRE-146). This connection declares
//!     `transactional_ddl: false`, so a failing script that changed the schema
//!     says the rollback did not reach it instead of claiming no changes were
//!     applied. Behind a `Capabilities` flag rather than a check against this
//!     one engine, since YugabyteDB does the same thing for its own reasons
//!     (FRE-91). Note what it is *not*: `transactions` stays true, because DML
//!     in a script really does roll back here.
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
//! ## Absent features, confirmed to fail sensibly rather than corrupt
//!
//! Declarative partitioning, `VACUUM`, range types, nested arrays, and
//! `COLLATE "C"` are all unsupported and surface as ordinary statement errors.
//! Cockroach also sends no error cursor position, so SQL editor errors carry
//! the message without the "(line L, column C)" suffix. The rest of the
//! Postgres suite passes against this container — point `HUBRO_PG_TEST_URL` at
//! it to re-run it there, minus the cases resting on those five features.

use hubro::db::{
    apply_staged, detect_row_identity, run_script, split_statements, Capabilities, DbPool, Filter,
    Internal, PageRequest, PgFlavor, Restriction, Rollback, RowIdentity, RowLocator, SortDir,
    StagedChange, TableKind, TableMeta, Value,
};

fn test_url() -> Option<String> {
    match std::env::var("HUBRO_CRDB_TEST_URL") {
        Ok(url) => Some(url),
        Err(_) => {
            eprintln!("skipping cockroachdb test: HUBRO_CRDB_TEST_URL not set");
            None
        }
    }
}

/// A parent/child pair with a real primary key, a foreign key and a secondary
/// index. Suffixed per test so the tests in this binary can run concurrently
/// against one database without fighting over one fixture.
async fn fresh_fixture(pool: &DbPool, suffix: &str) -> (String, String) {
    let readings = format!("readings_{suffix}");
    let sensors = format!("sensors_{suffix}");
    for sql in [
        format!("DROP TABLE IF EXISTS {readings} CASCADE"),
        format!("DROP TABLE IF EXISTS {sensors} CASCADE"),
        format!("CREATE TABLE {sensors} (id int PRIMARY KEY, name text NOT NULL)"),
        format!("INSERT INTO {sensors} (id, name) VALUES (1, 'alpha'), (2, 'beta')"),
        format!(
            "CREATE TABLE {readings} (
                id          int NOT NULL,
                sensor_id   int NOT NULL REFERENCES {sensors}(id),
                temperature double precision,
                note        text,
                PRIMARY KEY (id, sensor_id)
            )"
        ),
        format!("CREATE INDEX {readings}_by_temp ON {readings} (temperature)"),
        format!(
            "INSERT INTO {readings} (id, sensor_id, temperature)
             SELECT g, s.id, g * 1.5 FROM generate_series(1, 8) g CROSS JOIN {sensors} s"
        ),
    ] {
        pool.query(&sql).await.unwrap();
    }
    (readings, sensors)
}

fn find<'a>(tables: &'a [TableMeta], name: &str) -> &'a TableMeta {
    tables
        .iter()
        .find(|t| t.name == name && t.schema.as_deref() == Some("public"))
        .unwrap_or_else(|| panic!("{name} missing from introspection"))
}

fn integer(value: &Value) -> i64 {
    match value {
        Value::Integer(n) => *n,
        other => panic!("expected an integer cell, got {other:?}"),
    }
}

#[tokio::test]
async fn cockroach_identifies_itself_as_its_own_engine() {
    let Some(url) = test_url() else { return };
    let pool = DbPool::open_postgres(&url).await.unwrap();

    // Nothing else in this file depends on this answer, and that is
    // deliberate: every fix FRE-90 landed reads a catalog fact instead, so the
    // backend behaves correctly here whether or not it recognises the engine.
    // Detection exists to *report* who answered — and because FRE-92 needs it
    // to declare Materialize's capabilities, which no catalog fact supplies.
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
    let reserved: Vec<&TableMeta> = tables
        .iter()
        .filter(|t| {
            matches!(
                t.schema.as_deref(),
                Some("crdb_internal") | Some("pg_extension")
            )
        })
        .collect();
    assert!(
        reserved.len() > 50,
        "expected Cockroach's catalog schemas to be populated, got {}",
        reserved.len()
    );
    for table in &reserved {
        assert_eq!(table.internal, Some(Internal::System), "{table:?}");
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

    // ...and the user's own schema is untouched by the rule.
    for table in tables
        .iter()
        .filter(|t| t.schema.as_deref() == Some("public"))
    {
        assert_eq!(table.internal, None, "{table:?}");
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
        table: readings.clone(),
        limit: 3,
        offset: 0,
        sort: Some(("id".into(), SortDir::Desc)),
        filter: Some(Filter::equals("sensor_id", "2")),
        extra_key_column: None,
    };
    assert_eq!(pool.count_rows(&request).await.unwrap(), 8);

    let page = pool.fetch_page(&request).await.unwrap();
    assert_eq!(page.rows.len(), 3);
    let ids: Vec<i64> = page.rows.iter().map(|row| integer(&row[0])).collect();
    assert_eq!(ids, [8, 7, 6], "rows should come back highest id first");

    // Offsets keep walking the same ordering.
    let next = pool
        .fetch_page(&PageRequest {
            offset: 3,
            ..request
        })
        .await
        .unwrap();
    assert_eq!(integer(&next.rows[0][0]), 5);

    pool.close().await;
}

#[tokio::test]
async fn cockroach_rows_edit_through_the_composite_key() {
    let Some(url) = test_url() else { return };
    let pool = DbPool::open_postgres(&url).await.unwrap();
    let (readings, _) = fresh_fixture(&pool, "edit").await;

    let tables = pool.introspect().await.unwrap();
    let table = find(&tables, &readings);
    let identity = detect_row_identity(table, pool.dialect()).expect("composite pk");
    assert_eq!(
        identity,
        RowIdentity::PrimaryKey {
            columns: vec!["id".into(), "sensor_id".into()]
        }
    );

    let access = pool.backend_access(table);
    assert!(access.can_mutate());
    let key = || RowLocator {
        identity_values: vec![Value::Integer(1), Value::Integer(1)],
    };

    let counts = apply_staged(
        &pool,
        &access,
        table,
        &identity,
        &[StagedChange::Update {
            locator: key(),
            column: "note".into(),
            value: Value::Text("checked".into()),
        }],
    )
    .await
    .unwrap();
    assert_eq!(counts.updated_rows, 1);

    let counts = apply_staged(
        &pool,
        &access,
        table,
        &identity,
        &[StagedChange::Insert {
            columns: vec!["id".into(), "sensor_id".into(), "temperature".into()],
            values: vec![Value::Integer(99), Value::Integer(1), Value::Real(9.5)],
        }],
    )
    .await
    .unwrap();
    assert_eq!(counts.inserted_rows, 1);

    let counts = apply_staged(
        &pool,
        &access,
        table,
        &identity,
        &[StagedChange::Delete {
            locator: RowLocator {
                identity_values: vec![Value::Integer(99), Value::Integer(1)],
            },
        }],
    )
    .await
    .unwrap();
    assert_eq!(counts.deleted_rows, 1);

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
    pool.query("DROP TABLE IF EXISTS crdb_nokey CASCADE")
        .await
        .unwrap();
    pool.query("CREATE TABLE crdb_nokey (a int, b text)")
        .await
        .unwrap();
    pool.query("INSERT INTO crdb_nokey (a, b) VALUES (1, 'one'), (2, 'two')")
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

    pool.query("DROP TABLE crdb_nokey CASCADE").await.unwrap();
    pool.close().await;
}

#[tokio::test]
async fn cockroach_view_is_readable_but_never_editable() {
    let Some(url) = test_url() else { return };
    let pool = DbPool::open_postgres(&url).await.unwrap();
    let (readings, _) = fresh_fixture(&pool, "view").await;
    let view = format!("{readings}_warm");

    pool.query(&format!("DROP VIEW IF EXISTS {view}"))
        .await
        .unwrap();
    pool.query(&format!(
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
        .fetch_page(&PageRequest {
            schema: Some("public".into()),
            table: view.clone(),
            limit: 10,
            offset: 0,
            sort: Some(("id".into(), SortDir::Asc)),
            filter: None,
            extra_key_column: None,
        })
        .await
        .unwrap();
    assert!(!page.rows.is_empty(), "the view should have rows");

    pool.query(&format!("DROP VIEW {view}")).await.unwrap();
    pool.close().await;
}

#[tokio::test]
async fn cockroach_script_dml_rolls_back_but_its_ddl_does_not() {
    let Some(url) = test_url() else { return };
    let pool = DbPool::open_postgres(&url).await.unwrap();

    // The declaration that drives it, pinned separately from the behaviour: a
    // rollback here covers DML and nothing else, so `transactional_ddl` is the
    // one flag that differs from a full-featured engine. Declaring
    // `transactions: false` instead would be the worse lie — the DML half
    // below demonstrably works.
    assert_eq!(
        pool.backend_capabilities(),
        Capabilities {
            transactional_ddl: false,
            ..Capabilities::FULL
        }
    );

    // The engine behaviour, pinned so it stays deliberate. Cockroach's
    // `autocommit_before_ddl` defaults to on: it commits the open transaction
    // before each DDL statement, announcing it only as a NOTICE. A failing
    // script therefore rolls back its *writes* but leaves its schema changes
    // standing.
    //
    // hubro no longer claims otherwise: this connection declares
    // `transactional_ddl: false`, so a script containing DDL reports
    // `Rollback::ExceptSchemaChanges` rather than a flat "rolled back"
    // (FRE-146). What survives below is unchanged — the fix was to the claim,
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
    pool.query("DROP TABLE IF EXISTS crdb_script_ddl CASCADE")
        .await
        .unwrap();
    pool.query("DROP TABLE IF EXISTS crdb_script_dml CASCADE")
        .await
        .unwrap();
    pool.query("CREATE TABLE crdb_script_dml (id int PRIMARY KEY)")
        .await
        .unwrap();

    let sql = "INSERT INTO crdb_script_dml VALUES (1); \
               CREATE TABLE crdb_script_ddl (id int PRIMARY KEY); \
               SELECT * FROM missing_relation";
    let statements = split_statements(sql, pool.dialect());
    let error = run_script(&pool, pool.backend_capabilities(), &statements, |_| {})
        .await
        .expect_err("the final statement names no relation");
    assert_eq!(
        error.rollback,
        Rollback::ExceptSchemaChanges,
        "the rollback was real but did not reach the schema change, and hubro must say so"
    );

    // The auto-commit fires *before* the DDL, so it also commits everything
    // staged ahead of it: this INSERT is not itself a schema change and still
    // survives. That is the sharp edge — the escape isn't limited to the DDL
    // statement, it takes every write before it along.
    let rows = pool
        .query("SELECT count(*) FROM crdb_script_dml")
        .await
        .unwrap();
    assert_eq!(
        integer(&rows.rows[0][0]),
        1,
        "known gap: the commit before the DDL takes the preceding INSERT with it"
    );

    // ...as does the schema change itself.
    let survivors = pool
        .query(
            "SELECT count(*) FROM information_schema.tables \
             WHERE table_name = 'crdb_script_ddl'",
        )
        .await
        .unwrap();
    assert_eq!(
        integer(&survivors.rows[0][0]),
        1,
        "known gap: Cockroach auto-commits DDL out of the transaction"
    );

    pool.query("DROP TABLE IF EXISTS crdb_script_ddl CASCADE")
        .await
        .unwrap();
    pool.query("DROP TABLE crdb_script_dml CASCADE")
        .await
        .unwrap();
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
    // describe a table that does not exist, which is the same false claim
    // FRE-146 removed, pointing the other way.
    pool.query("DROP TABLE IF EXISTS crdb_unreached_ddl CASCADE")
        .await
        .unwrap();
    pool.query("DROP TABLE IF EXISTS crdb_unreached_dml CASCADE")
        .await
        .unwrap();
    pool.query("CREATE TABLE crdb_unreached_dml (id int PRIMARY KEY)")
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
        .query("SELECT count(*) FROM crdb_unreached_dml")
        .await
        .unwrap();
    assert_eq!(
        integer(&rows.rows[0][0]),
        0,
        "the INSERT rolled back — no DDL ran to commit it early"
    );
    let survivors = pool
        .query(
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

    pool.query("DROP TABLE crdb_unreached_dml CASCADE")
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
    // "no changes were applied" for exactly this shape, which is the FRE-146
    // bug returning by a narrower route.
    pool.query("DROP TABLE IF EXISTS crdb_boundary_dup CASCADE")
        .await
        .unwrap();
    pool.query("DROP TABLE IF EXISTS crdb_boundary_dml CASCADE")
        .await
        .unwrap();
    pool.query("CREATE TABLE crdb_boundary_dml (id int PRIMARY KEY)")
        .await
        .unwrap();
    // Already present, so the script's CREATE TABLE is the statement that fails.
    pool.query("CREATE TABLE crdb_boundary_dup (id int PRIMARY KEY)")
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
        .query("SELECT count(*) FROM crdb_boundary_dml")
        .await
        .unwrap();
    assert_eq!(
        integer(&rows.rows[0][0]),
        1,
        "the autocommit fired before the failing DDL and took the INSERT with it"
    );

    pool.query("DROP TABLE crdb_boundary_dup CASCADE")
        .await
        .unwrap();
    pool.query("DROP TABLE crdb_boundary_dml CASCADE")
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
    pool.query("DROP TABLE IF EXISTS crdb_serial CASCADE")
        .await
        .unwrap();
    pool.query("CREATE TABLE crdb_serial (id serial PRIMARY KEY, name text)")
        .await
        .unwrap();
    pool.query("INSERT INTO crdb_serial (name) VALUES ('a'), ('b')")
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
        .query("SELECT id FROM crdb_serial ORDER BY id")
        .await
        .unwrap();
    let ids: Vec<i64> = rows.rows.iter().map(|row| integer(&row[0])).collect();
    assert_eq!(ids.len(), 2);
    assert!(
        ids[0] > 1_000_000,
        "serial values are unique_rowid()-shaped, not 1 and 2: {ids:?}"
    );

    pool.query("DROP TABLE crdb_serial CASCADE").await.unwrap();
    pool.close().await;
}
