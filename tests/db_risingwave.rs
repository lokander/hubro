//! RisingWave verification (FRE-93). Like Materialize (`tests/db_materialize.rs`)
//! it is a streaming engine speaking the Postgres wire protocol — but a more
//! capable one in every respect except the one that matters most for writing:
//! its tables take primary keys and ordinary DML, and it has no transactions at
//! all.
//!
//! Needs a running server (Docker only, per CLAUDE.md) and is skipped unless
//! `HUBRO_RISINGWAVE_TEST_URL` is set, e.g.:
//!
//! ```sh
//! docker run -d --name hubro-risingwave-test -p 4566:4566 \
//!   risingwavelabs/risingwave:latest single_node
//! HUBRO_RISINGWAVE_TEST_URL='postgres://root@localhost:4566/dev' cargo test
//! ```
//!
//! No password and no TLS; the default database is `dev`.
//!
//! ## The finding that matters: no transactions
//!
//! At the wire level `BEGIN` raises `Read-write transaction is not supported
//! yet` as a *notice* and carries on, and `ROLLBACK` reports no transaction in
//! progress. Through sqlx it is less quiet than that: `pool.begin()` sees the
//! connection never entered a transaction and fails with `got unexpected
//! connection status after attempting to begin transaction`, pinned in
//! `risingwave_refuses_to_begin_a_transaction`.
//!
//! **So nothing was ever writing unguarded** — worth stating plainly, because
//! the opposite is the easy assumption. `execute_all_checked` opens its
//! transaction before running anything, so a staged save on this engine failed
//! at the first step with every guarantee intact.
//!
//! What was wrong was what the user saw. Every cell edit and every
//! multi-statement script died on a sentence about connection status that names
//! nothing they can act on, after they had staged the work. RisingWave is
//! therefore the first backend to declare something other than
//! `Capabilities::FULL` — `transactions: false` — which is what FRE-87 built
//! the model for. Two things follow automatically and are pinned below:
//! `wrap_atomically` stops wrapping multi-statement scripts, so they run
//! sequentially and report honestly; and every object resolves to read-only
//! with [`NO_GUARDED_WRITE`] as the reason, up front rather than at save time.
//!
//! Editing is refused rather than offered unguarded. That is a real loss of
//! function, taken deliberately: hubro's editing design rests on the row-count
//! guard behind `execute_all_checked`, which commits only when a statement
//! affected exactly the rows it expected — and that guard is a transaction. An
//! engine that cannot support the claim should not be handed the claim.
//!
//! ## Writes are not read-your-own
//!
//! A row inserted here is not visible to an immediately following `SELECT`;
//! it appears about a second later, once the write has passed a barrier. That
//! is ordinary for a streaming engine and nothing hubro can fix, but it is
//! squarely a *viewer* problem — refreshing the grid right after a write can
//! legitimately not show it. Pinned in
//! `risingwave_writes_are_not_visible_to_an_immediate_read` so it is a known
//! property rather than an intermittent mystery, and every assertion in this
//! file that reads back a write settles first.
//!
//! ## What needed fixing, and helped everyone
//!
//! Both introspection failures were hubro asking for PostgreSQL internals it
//! did not need:
//!
//!  1. **`NULL::name`** in the internal-objects query. `name` is a
//!     PostgreSQL-internal type and RisingWave has none, so it could not even
//!     bind the cast — the whole schema tree died. Now `NULL::text`, which
//!     unions with `name` everywhere that has both.
//!  2. **`CROSS JOIN LATERAL unnest(…)`** in the index query. PostgreSQL
//!     implies `LATERAL` for a set-returning function in `FROM`, so the keyword
//!     was decoration; RisingWave's parser wants a subquery after it and
//!     refused to prepare the statement. Dropped, and verified equivalent on
//!     all seven engines.
//!
//! Neither is a RisingWave special case — both make the shared query rest on
//! less.
//!
//! ## Absent, and failing sensibly
//!
//! No `SERIAL`, no `CREATE TYPE`/enums, no `DROP TYPE`, no declarative
//! partitioning, and dates outside the ordinary range are rejected rather than
//! stored. Each surfaces as an ordinary statement error.
//!
//! ## The engine's own catalog, and its streaming edges
//!
//! `rw_catalog` holds 74 objects listed exactly like a user schema, so it would
//! otherwise be most of the schema tree — the same problem CockroachDB (119)
//! and Materialize (265) each posed. 61 of the 74 report `table_type =
//! 'SYSTEM TABLE'`, which is now classified as the engine's own alongside
//! `SYSTEM VIEW` and which no other engine emits outside `pg_catalog`; the
//! other 13 are plain `VIEW`, indistinguishable from a user's, so the reserved
//! schema name identifies them, gated on the flavor.
//!
//! Sources and sinks are RisingWave's other objects with no Postgres
//! equivalent. Both are views rather than tables now: a `SOURCE` is written by
//! the engine from outside, and a `SINK` writes *outward*. The source's reason
//! is worded for the object rather than the engine, since Materialize reports
//! `SOURCE` too and was previously naming itself in a message RisingWave also
//! showed.
//!
//! **A sink still cannot be opened**, and that half is not fixed. It has no
//! rows at all, so `fetch_page` gets the engine's own `table or source not
//! found`; being a view stops it offering editing, but views are browsable and
//! nothing here says "this object has no rows to browse". Suppressing the open
//! needs a per-object *read* gate, and `read_query` is connection-level — that
//! is an extension of the capability model rather than a verification finding,
//! so it is recorded here and filed as FRE-148 rather than bolted on.

use hubro::db::{
    apply_staged, detect_row_identity, run_script, split_statements, Capabilities, DbPool, Filter,
    Internal, PageRequest, PgFlavor, Restriction, Rollback, RowIdentity, RowLocator, SortDir,
    StagedChange, TableKind, TableMeta, Value, NO_GUARDED_WRITE,
};

fn test_url() -> Option<String> {
    match std::env::var("HUBRO_RISINGWAVE_TEST_URL") {
        Ok(url) => Some(url),
        Err(_) => {
            eprintln!("skipping risingwave test: HUBRO_RISINGWAVE_TEST_URL not set");
            None
        }
    }
}

/// A keyed table plus a materialized view over it. Suffixed per test so the
/// tests in this binary can run concurrently against one database.
async fn fresh_fixture(pool: &DbPool, suffix: &str) -> (String, String) {
    let readings = format!("rw_readings_{suffix}");
    let matview = format!("rw_avg_{suffix}");
    for sql in [
        format!("DROP MATERIALIZED VIEW IF EXISTS {matview}"),
        format!("DROP TABLE IF EXISTS {readings}"),
        // A real PRIMARY KEY, unlike Materialize which refuses one — this is
        // the engine that would be editable if it could guarantee the write.
        format!(
            "CREATE TABLE {readings} (
                id          int,
                sensor_id   int,
                temperature double precision,
                note        varchar,
                PRIMARY KEY (id, sensor_id)
            )"
        ),
        format!(
            "INSERT INTO {readings} VALUES
                (1, 1, 1.5, 'a'), (2, 1, 3.0, 'b'), (3, 1, 4.5, 'c'),
                (4, 2, 6.0, 'd'), (5, 2, 7.5, 'e')"
        ),
        format!(
            "CREATE MATERIALIZED VIEW {matview} AS
             SELECT sensor_id, avg(temperature) AS avg_temp FROM {readings} GROUP BY sensor_id"
        ),
    ] {
        pool.query(&sql).await.unwrap();
    }
    (readings, matview)
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
async fn risingwave_is_not_mistaken_for_stock_postgres() {
    let Some(url) = test_url() else { return };
    let pool = DbPool::open_postgres(&url).await.unwrap();

    // Load-bearing here in a way it was not for CockroachDB or YugabyteDB: the
    // capability declaration below is keyed on this answer, so a detection
    // regression would silently restore unguarded editing.
    assert_eq!(pool.pg_flavor(), Some(PgFlavor::RisingWave));

    pool.close().await;
}

/// Waits for a write to become visible. See the header: RisingWave applies DML
/// through a barrier, so a read issued immediately after a write can miss it.
async fn settle(pool: &DbPool, sql: &str, expected: i64) {
    for _ in 0..40 {
        let rows = pool.query(sql).await.unwrap();
        if integer(&rows.rows[0][0]) == expected {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    }
    let rows = pool.query(sql).await.unwrap();
    panic!(
        "never settled: {sql} gave {:?}, expected {expected}",
        rows.rows[0][0]
    );
}

#[tokio::test]
async fn risingwave_refuses_to_begin_a_transaction() {
    let Some(url) = test_url() else { return };
    let pool = DbPool::open_postgres(&url).await.unwrap();

    // The engine fact everything else in this file rests on, asserted against
    // the server rather than taken from its documentation — and asserted in
    // the form hubro actually meets. RisingWave answers `BEGIN` with a notice
    // rather than an error, but the driver notices the connection never
    // entered a transaction and fails there.
    //
    // This is the reassuring half of the finding: a staged save opens its
    // transaction *before* running any statement, so on this engine it failed
    // with every guarantee intact rather than writing something it could not
    // take back. What the declaration fixes is the message and the timing, not
    // a hole.
    let error = pool
        .begin_script_tx()
        .await
        .err()
        .expect("RisingWave has no read-write transactions");
    let message = error.message().to_lowercase();
    assert!(
        message.contains("transaction"),
        "expected the failure to name transactions, got: {message}"
    );

    pool.close().await;
}

#[tokio::test]
async fn risingwave_writes_are_not_visible_to_an_immediate_read() {
    let Some(url) = test_url() else { return };
    let pool = DbPool::open_postgres(&url).await.unwrap();

    // Recorded because it is a viewer-visible property rather than a hubro
    // bug: a refresh issued straight after a write can legitimately not show
    // it. Asserted only as "it arrives", never as "it is missing at first" —
    // the barrier is a timing detail, and a test that required the row to be
    // absent would be a test that fails on a slow enough machine.
    let table = "rw_visibility_probe";
    pool.query(&format!("DROP TABLE IF EXISTS {table}"))
        .await
        .unwrap();
    pool.query(&format!("CREATE TABLE {table} (id int, PRIMARY KEY (id))"))
        .await
        .unwrap();
    pool.execute(&format!("INSERT INTO {table} VALUES (1)"))
        .await
        .unwrap();

    settle(&pool, &format!("SELECT count(*) FROM {table}"), 1).await;

    pool.query(&format!("DROP TABLE {table}")).await.unwrap();
    pool.close().await;
}

#[tokio::test]
async fn risingwave_declares_itself_non_transactional() {
    let Some(url) = test_url() else { return };
    let pool = DbPool::open_postgres(&url).await.unwrap();

    // The first backend in the project to declare anything narrower than FULL.
    let caps = pool.backend_capabilities();
    assert!(!caps.transactions);
    // ...and narrower only about transactions. It queries, writes, runs DDL and
    // pages by LIMIT/OFFSET like any other; overstating the loss would disable
    // the SQL editor's write paths, which work.
    //
    // `transactional_ddl` goes with it (FRE-146) rather than being a second,
    // independent loss: that flag asks what a *rollback* covers, and nothing
    // here rolls back at all. Unlike CockroachDB and YugabyteDB — which have
    // real transactions and lose only the schema half — the two are not
    // separable on this engine.
    assert_eq!(
        caps,
        Capabilities {
            transactions: false,
            transactional_ddl: false,
            ..Capabilities::FULL
        }
    );

    pool.close().await;
}

#[tokio::test]
async fn risingwave_keyed_table_is_refused_for_editing_with_the_reason() {
    let Some(url) = test_url() else { return };
    let pool = DbPool::open_postgres(&url).await.unwrap();
    let (readings, _) = fresh_fixture(&pool, "edit").await;

    let tables = pool.introspect().await.unwrap();
    let table = find(&tables, &readings);

    // The row *is* addressable — this table has a composite primary key, and
    // on any other engine it would be editable. What is missing is the ability
    // to take the write back if the guard fires.
    assert_eq!(
        detect_row_identity(table, pool.dialect()),
        Some(RowIdentity::PrimaryKey {
            columns: vec!["id".into(), "sensor_id".into()]
        })
    );

    let access = pool.backend_access(table);
    assert!(!access.can_mutate());
    assert_eq!(
        access.restriction,
        Some(Restriction::Declared(NO_GUARDED_WRITE))
    );
    // The identity survives the refusal: cell fetch still pins a row by it.
    assert!(access.identity.is_some());

    // And the write path refuses too, rather than relying on the UI to have
    // hidden the button.
    let refused = apply_staged(
        &pool,
        &access,
        table,
        &access.identity.clone().unwrap(),
        &[StagedChange::Update {
            locator: RowLocator {
                identity_values: vec![Value::Integer(1), Value::Integer(1)],
            },
            column: "note".into(),
            value: Value::Text("edited".into()),
        }],
    )
    .await
    .expect_err("a staged write must not run unguarded");
    assert!(
        refused.message.contains("transactions"),
        "the refusal should name the missing capability, got: {}",
        refused.message
    );

    // Nothing was written.
    let rows = pool
        .query(&format!(
            "SELECT note FROM {readings} WHERE id = 1 AND sensor_id = 1"
        ))
        .await
        .unwrap();
    assert_eq!(rows.rows[0][0], Value::Text("a".into()));

    pool.close().await;
}

#[tokio::test]
async fn risingwave_scripts_run_sequentially_instead_of_claiming_atomicity() {
    let Some(url) = test_url() else { return };
    let pool = DbPool::open_postgres(&url).await.unwrap();

    // The other half that falls out of the declaration: `wrap_atomically`
    // returns false, so a failing multi-statement script reports
    // `Rollback::None` and leaves the earlier statement standing. That is
    // the truth on this engine, and saying it is the point — the alternative
    // is a rollback message that means nothing.
    let table = "rw_script_probe";
    pool.query(&format!("DROP TABLE IF EXISTS {table}"))
        .await
        .unwrap();
    pool.query(&format!("CREATE TABLE {table} (id int, PRIMARY KEY (id))"))
        .await
        .unwrap();

    let sql = format!(
        "INSERT INTO {table} VALUES (1); \
         SELECT * FROM rw_missing_relation"
    );
    let statements = split_statements(&sql, pool.dialect());
    let error = run_script(&pool, pool.backend_capabilities(), &statements, |_| {})
        .await
        .expect_err("the second statement names no relation");
    assert_eq!(
        error.rollback,
        Rollback::None,
        "a non-transactional backend must not claim to have rolled back"
    );

    // Settles first: the INSERT standing is the claim, not that it is instantly
    // visible (see the header).
    settle(&pool, &format!("SELECT count(*) FROM {table}"), 1).await;

    pool.query(&format!("DROP TABLE {table}")).await.unwrap();
    pool.close().await;
}

#[tokio::test]
async fn risingwave_introspects_tables_keys_and_materialized_views() {
    let Some(url) = test_url() else { return };
    let pool = DbPool::open_postgres(&url).await.unwrap();
    let (readings, matview) = fresh_fixture(&pool, "intro").await;

    // The regression guard for both query fixes: without either, this call
    // fails and nothing below is reachable.
    let tables = pool.introspect().await.unwrap();

    let table = find(&tables, &readings);
    assert_eq!(table.kind, TableKind::Table);
    assert_eq!(table.internal, None);
    let columns: Vec<&str> = table.columns.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(columns, ["id", "sensor_id", "temperature", "note"]);
    let pk: Vec<&str> = table
        .primary_key()
        .iter()
        .map(|c| c.name.as_str())
        .collect();
    assert_eq!(pk, ["id", "sensor_id"]);

    assert_eq!(find(&tables, &matview).kind, TableKind::MaterializedView);

    // The user's own objects are not the engine's.
    assert_eq!(table.internal, None);
    assert_eq!(find(&tables, &matview).internal, None);

    pool.close().await;
}

#[tokio::test]
async fn risingwave_pages_sorts_and_filters_over_a_materialized_view() {
    let Some(url) = test_url() else { return };
    let pool = DbPool::open_postgres(&url).await.unwrap();
    let (readings, matview) = fresh_fixture(&pool, "page").await;

    // The table first: ordinary paging still has to work on a backend whose
    // writes are refused, since browsing is most of what it will be used for.
    let request = PageRequest {
        schema: Some("public".into()),
        table: readings.clone(),
        limit: 2,
        offset: 0,
        sort: Some(("id".into(), SortDir::Desc)),
        filter: Some(Filter::equals("sensor_id", "1")),
        extra_key_column: None,
    };
    assert_eq!(pool.count_rows(&request).await.unwrap(), 3);
    let page = pool.fetch_page(&request).await.unwrap();
    let ids: Vec<i64> = page.rows.iter().map(|row| integer(&row[0])).collect();
    assert_eq!(ids, [3, 2]);

    // Then the materialized view, which is what the issue asked for and what
    // most objects on a streaming engine are.
    let mv_request = PageRequest {
        schema: Some("public".into()),
        table: matview.clone(),
        limit: 10,
        offset: 0,
        sort: Some(("sensor_id".into(), SortDir::Asc)),
        filter: Some(Filter::equals("sensor_id", "2")),
        extra_key_column: None,
    };
    let mv_page = pool.fetch_page(&mv_request).await.unwrap();
    assert_eq!(mv_page.rows.len(), 1);
    assert_eq!(integer(&mv_page.rows[0][0]), 2);

    pool.close().await;
}

#[tokio::test]
async fn risingwave_own_catalog_is_marked_as_the_engines_own() {
    let Some(url) = test_url() else { return };
    let pool = DbPool::open_postgres(&url).await.unwrap();
    fresh_fixture(&pool, "catalog").await;

    let tables = pool.introspect().await.unwrap();

    // Selected by schema even though the backend reaches most of these through
    // `table_type`, so the test names the objects independently of the rule
    // under test. The previous version of this test asserted that *nothing*
    // was internal and passed for the wrong reason — it could not fail in
    // either state, and hid 74 unmarked objects.
    let engine: Vec<&TableMeta> = tables
        .iter()
        .filter(|t| t.schema.as_deref() == Some("rw_catalog"))
        .collect();
    assert!(
        engine.len() > 50,
        "expected RisingWave's catalog to be populated, got {}",
        engine.len()
    );
    for table in &engine {
        assert_eq!(table.internal, Some(Internal::System), "{table:?}");
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
async fn risingwave_source_and_sink_are_read_only_and_say_which_they_are() {
    let Some(url) = test_url() else { return };
    let pool = DbPool::open_postgres(&url).await.unwrap();

    // Both object kinds the issue named, created rather than assumed. The
    // `blackhole` sink connector and the `datagen` source need no external
    // system, which is what makes them testable here at all.
    for sql in [
        "DROP SINK IF EXISTS rw_edge_sink",
        "DROP SOURCE IF EXISTS rw_edge_source",
        "DROP TABLE IF EXISTS rw_edge_base",
        "CREATE TABLE rw_edge_base (id int, PRIMARY KEY (id))",
        "CREATE SINK rw_edge_sink FROM rw_edge_base WITH (connector = 'blackhole')",
        "CREATE SOURCE rw_edge_source (v1 int) \
         WITH (connector = 'datagen', datagen.rows.per.second = '1') \
         FORMAT PLAIN ENCODE JSON",
    ] {
        pool.query(sql).await.unwrap();
    }

    let tables = pool.introspect().await.unwrap();

    let source = find(&tables, "rw_edge_source");
    assert_eq!(source.kind, TableKind::View);
    assert_eq!(source.kind_label.as_deref(), Some("source"));
    assert!(!pool.backend_access(source).can_mutate());
    let notice = pool.backend_access(source).read_only_notice().unwrap();
    assert!(
        notice.contains("written by the engine") && !notice.contains("Materialize"),
        "a source's reason must describe the object, not another engine: {notice}"
    );

    // The sink is the object that previously read as an ordinary table: it has
    // no rows at all, so offering it for browsing produced the engine's own
    // "table or source not found" on open.
    let sink = find(&tables, "rw_edge_sink");
    assert_eq!(sink.kind, TableKind::View);
    assert_eq!(sink.kind_label.as_deref(), Some("sink"));
    assert!(!pool.backend_access(sink).can_mutate());
    assert!(pool
        .backend_access(sink)
        .read_only_notice()
        .unwrap()
        .contains("no rows of its own"));

    for sql in [
        "DROP SINK rw_edge_sink",
        "DROP SOURCE rw_edge_source",
        "DROP TABLE rw_edge_base",
    ] {
        pool.query(sql).await.unwrap();
    }
    pool.close().await;
}
