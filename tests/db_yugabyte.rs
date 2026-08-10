//! YugabyteDB verification (FRE-91). Yugabyte runs the *real* PostgreSQL query
//! layer on top of its own distributed storage, so unlike CockroachDB
//! (`tests/db_cockroach.rs`) it is not a reimplementation — `pg_catalog` is
//! genuine, and introspection needed no changes to read it correctly. These
//! tests pin the places where the storage layer underneath still shows
//! through, and the one place it does so sharply enough to want a fix.
//!
//! Needs a running server (Docker only, per CLAUDE.md) and is skipped unless
//! `HUBRO_YUGABYTE_TEST_URL` is set, e.g.:
//!
//! ```sh
//! docker run -d --name hubro-yugabyte-test -p 5436:5433 -p 15433:15433 \
//!   yugabytedb/yugabyte:latest bin/yugabyted start --background=false
//! docker exec hubro-yugabyte-test bash -c \
//!   '/home/yugabyte/bin/ysqlsh -h $(hostname -i) -U yugabyte -c "CREATE DATABASE demo"'
//! HUBRO_YUGABYTE_TEST_URL='postgres://yugabyte@localhost:5436/demo' cargo test
//! ```
//!
//! YSQL listens on 5433 *inside* the container, mapped to 5436 to stay clear of
//! the Timescale container on 5434. The stock image uses trust auth, so the URL
//! carries no password. The `bash -c` wrapper is load-bearing: `ysqlsh` binds
//! the container's own address rather than loopback, and an unwrapped
//! `$(hostname -i)` would expand on the *host* instead.
//!
//! ## What the verification found
//!
//! **No backend change was needed to make it work.** Run with
//! `-- --test-threads=1`, the whole Postgres suite passes against this
//! container — including every DDL test, which is more than CockroachDB
//! managed — with three exceptions, all of them the engine or the container
//! (findings 3-5 below).
//!
//! The other two findings are not shared-suite failures at all under that
//! invocation: they appear only when schema changes run *concurrently*, which
//! is what removing `--test-threads=1` does. One of the two was a real
//! robustness gap in hubro, filed here and since fixed (finding 2).
//!
//!  1. **Concurrent DDL is refused.** Two `CREATE TABLE`s in flight at once
//!     fail with `could not serialize access due to concurrent update`, because
//!     each DDL bumps a cluster-wide catalog version and Yugabyte will not
//!     replay a statement that is not first in its batch. This is the one
//!     finding with teeth, and it is mostly a *test harness* problem: the
//!     shared suite runs its tests concurrently against one database, so it
//!     needs `--test-threads=1` here. hubro reaches it far more rarely, but it
//!     *can*: run slots are claimed per connection (`claim_run_slot` in
//!     `src/ui/state/sql.rs`), so two open connections can run DDL at once, and
//!     a cancelled Postgres statement keeps executing server-side — so
//!     cancel-then-rerun can overlap two DDLs on one connection. What makes
//!     this a non-issue is the *shape* of the failure, which
//!     `yugabyte_refuses_concurrent_ddl_without_corrupting_anything` pins: a
//!     plain statement error carrying the engine's own explanation, with the
//!     winner fully applied. This file serialises its own fixture DDL through
//!     [`ddl_lock`] so it stays honest under the default threaded runner.
//!  2. **Introspection can fail transiently — since fixed (FRE-147).**
//!     `MISMATCHED_SCHEMA` — "the catalog snapshot used for this transaction
//!     has been invalidated" — surfaces when a schema change lands between the
//!     six queries introspection runs. It is a *read* failing, which is the
//!     part that matters: refreshing the schema tree while *anything* is
//!     changing the schema can error out, and the message names a Yugabyte
//!     internal rather than anything the user can act on. Not only a
//!     multi-user hazard, either — the two routes in finding 1 reach it from a
//!     single hubro window. Nor is it rare: instrumenting a retry here showed
//!     it firing on roughly half of this binary's runs, with the second
//!     attempt succeeding every time.
//!
//!     hubro now retries it once itself, so the tests below call
//!     [`DbPool::introspect`] directly and the `introspect_stable` helper they
//!     used to go through is gone. Yugabyte raises this as SQLSTATE `40001`
//!     (`ERRCODE_T_R_SERIALIZATION_FAILURE`), which is why the fix classifies
//!     on the code rather than on `MISMATCHED_SCHEMA` — the same code carries
//!     CockroachDB's retryable conflicts, and no engine-internal string has to
//!     be matched.
//!  3. **A failing script does not roll back its DDL — since reported
//!     honestly (FRE-146).** The same behaviour CockroachDB has, for a
//!     different reason: Yugabyte commits each schema change as it executes.
//!     DML in the same transaction rolls back correctly, so the fix narrowed
//!     what hubro *claims* rather than disabling transactions — this
//!     connection declares `transactional_ddl: false` and `transactions: true`,
//!     and a failing script that changed the schema now says the rollback did
//!     not cover it. Unlike CockroachDB, the DML written before the schema
//!     change *is* undone here, which is why the message hubro shows names only
//!     what it is certain of.
//!  4. **`numeric 'NaN'` is not storable** (`DECIMAL does not support NaN
//!     yet`), so the shared suite's undecodable-cell fixture cannot be built
//!     here. The decode path it exercises is engine-independent.
//!  5. **The stock image uses trust auth**, so the shared suite's
//!     wrong-password test cannot fail the way it expects. A container
//!     property, not an engine one.
//!
//! Note that `stock_postgres_is_detected_as_stock_postgres` in
//! `tests/db_postgres.rs` also fails when the shared suite is pointed here, and
//! should: it is asserting that this connection is *not* mistaken for stock
//! PostgreSQL, which is the whole point of the detection.
//!
//! ## Scope of this file
//!
//! Only what Yugabyte does differently. Types and export are verified by
//! pointing the shared suite here (`db_export`, `db_staged` and the type cases
//! in `db_postgres` all pass), not by anything below — there is no divergence
//! for them to pin.
//!
//! ## Where the storage layer shows through, harmlessly
//!
//! Indexes are LSM rather than B-tree and carry sharding attributes, but
//! `pg_index` is byte-identical to stock Postgres — the extra detail lives in
//! `pg_get_indexdef` (`USING lsm (id HASH, sensor_id ASC)`), so it reaches Show
//! DDL (FRE-108) and leaves the browsable metadata untouched. Neither the
//! access method nor the distribution is anything hubro records, so nothing
//! had to change to accommodate them.
//!
//! A table declared without a primary key has no user-visible key at all — no
//! `ctid`, and no implicit `rowid` of the kind CockroachDB adds — so it is
//! read-only, exactly as on stock Postgres.

use std::sync::OnceLock;

use hubro::db::{
    apply_staged, detect_row_identity, run_script, split_statements, Capabilities, DbPool,
    DdlObject, DdlSource, Filter, Internal, PageRequest, PgFlavor, Restriction, Rollback,
    RowIdentity, RowLocator, SortDir, StagedChange, TableKind, TableMeta, Value,
};
use tokio::sync::Mutex;

fn test_url() -> Option<String> {
    match std::env::var("HUBRO_YUGABYTE_TEST_URL") {
        Ok(url) => Some(url),
        Err(_) => {
            eprintln!("skipping yugabyte test: HUBRO_YUGABYTE_TEST_URL not set");
            None
        }
    }
}

/// Serialises schema changes across the tests in this binary.
///
/// Yugabyte refuses concurrent DDL outright (finding 1 in the header): each
/// statement bumps a cluster-wide catalog version, and two in flight leave one
/// of them unable to replay. Without this the tests below fail on each other's
/// fixtures rather than on anything they are testing — and the failure looks
/// like a hubro bug, which is exactly the confusion this verification exists to
/// prevent.
///
/// Held for fixture setup only, never across the assertions, so the tests still
/// exercise concurrent reads and writes. Ordinary data reads are unaffected by
/// a racing schema change; catalog reads are not, and hubro's own retry
/// (finding 2) is what carries them past it.
fn ddl_lock() -> &'static Mutex<()> {
    static DDL_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    DDL_LOCK.get_or_init(|| Mutex::new(()))
}

/// A parent/child pair with a composite primary key, a foreign key and a
/// secondary index. Suffixed per test so each test owns its objects.
async fn fresh_fixture(pool: &DbPool, suffix: &str) -> (String, String) {
    let readings = format!("yb_readings_{suffix}");
    let sensors = format!("yb_sensors_{suffix}");
    let guard = ddl_lock().lock().await;
    for sql in [
        format!("DROP TABLE IF EXISTS {readings} CASCADE"),
        format!("DROP TABLE IF EXISTS {sensors} CASCADE"),
        format!("CREATE TABLE {sensors} (id int PRIMARY KEY, name text NOT NULL)"),
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
    ] {
        pool.query(&sql).await.unwrap();
    }
    drop(guard);

    // Data loads outside the lock: these are ordinary writes, and Yugabyte has
    // no trouble running them concurrently.
    for sql in [
        format!("INSERT INTO {sensors} (id, name) VALUES (1, 'alpha'), (2, 'beta')"),
        format!(
            "INSERT INTO {readings} (id, sensor_id, temperature)
             SELECT g, s.id, g * 1.5 FROM generate_series(1, 8) g CROSS JOIN {sensors} s"
        ),
    ] {
        pool.query(&sql).await.unwrap();
    }
    (readings, sensors)
}

/// Runs schema changes that aren't part of [`fresh_fixture`], under the lock.
async fn with_ddl(pool: &DbPool, statements: &[String]) {
    let _guard = ddl_lock().lock().await;
    for sql in statements {
        pool.query(sql).await.unwrap();
    }
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
async fn yugabyte_is_not_mistaken_for_stock_postgres() {
    let Some(url) = test_url() else { return };
    let pool = DbPool::open_postgres(&url).await.unwrap();

    // Yugabyte's `version()` *leads* with `PostgreSQL 15.12-…`, because the
    // query layer genuinely is PostgreSQL 15 — so a detector that matched the
    // claimed Postgres version before looking for the engine's own name would
    // file this as stock. Nothing branches on the answer today (FRE-90), but
    // FRE-92 will, and this is the input most likely to break it.
    assert_eq!(pool.pg_flavor(), Some(PgFlavor::Yugabyte));

    pool.close().await;
}

#[tokio::test]
async fn yugabyte_introspects_with_full_postgres_parity() {
    let Some(url) = test_url() else { return };
    let pool = DbPool::open_postgres(&url).await.unwrap();
    let (readings, sensors) = fresh_fixture(&pool, "intro").await;

    let tables = pool.introspect().await.unwrap();
    let table = find(&tables, &readings);

    assert_eq!(table.kind, TableKind::Table);
    assert_eq!(table.internal, None);
    let columns: Vec<&str> = table.columns.iter().map(|c| c.name.as_str()).collect();
    // No implicit key column — contrast CockroachDB, which adds a `rowid` here.
    assert_eq!(columns, ["id", "sensor_id", "temperature", "note"]);

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

    // Yugabyte's indexes are LSM rather than B-tree, and the primary key
    // doubles as the table's distribution key. Neither is anything the
    // browsable metadata records, so both arrive as ordinary index metadata —
    // which is the claim being pinned.
    assert!(
        table
            .indexes
            .iter()
            .any(|i| i.columns == ["temperature"] && !i.unique),
        "expected the secondary index, got {:?}",
        table.indexes
    );

    // The sharding attributes FRE-91 asked about do exist — they just live in
    // the DDL rather than in `pg_index`, which is byte-identical to stock
    // Postgres here. `pg_get_indexdef` renders them, so Show DDL (FRE-108)
    // reports the distribution the user actually has instead of a B-tree that
    // was never created. Native output, so this is the server's own text.
    let ddl = pool
        .fetch_ddl(table, &DdlObject::Index(format!("{readings}_pkey")))
        .await
        .unwrap();
    // Asserted rather than assumed: `pg_get_indexdef` is what carries the
    // sharding attributes, and a fall back to reconstruction would drop them
    // while still producing plausible-looking DDL.
    assert_eq!(ddl.source, DdlSource::Native);
    let text = ddl.text();
    assert!(
        text.contains("USING lsm") && text.contains("HASH"),
        "expected Yugabyte's sharding attributes in the index DDL, got: {text}"
    );

    pool.close().await;
}

#[tokio::test]
async fn yugabyte_pages_sorts_and_filters() {
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
    // Rows are distributed by hash, so the storage order is arbitrary; an
    // ORDER BY has to impose the ordering rather than reflect it.
    assert_eq!(ids, [8, 7, 6]);

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
async fn yugabyte_rows_edit_through_the_composite_key() {
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

    let counts = apply_staged(
        &pool,
        &access,
        table,
        &identity,
        &[StagedChange::Update {
            locator: RowLocator {
                identity_values: vec![Value::Integer(1), Value::Integer(1)],
            },
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
async fn yugabyte_table_without_a_key_is_read_only_like_postgres() {
    let Some(url) = test_url() else { return };
    let pool = DbPool::open_postgres(&url).await.unwrap();

    // The interesting contrast with CockroachDB, which gives such a table an
    // implicit `rowid` and leaves it editable. Yugabyte addresses rows by an
    // internal `ybctid` that plain SQL cannot select, so there is nothing to
    // write through — the same answer stock Postgres gives, reached the same
    // way. Browsing still works; only editing is refused, with a reason.
    with_ddl(
        &pool,
        &[
            "DROP TABLE IF EXISTS yb_nokey CASCADE".to_string(),
            "CREATE TABLE yb_nokey (a int, b text)".to_string(),
        ],
    )
    .await;
    pool.query("INSERT INTO yb_nokey (a, b) VALUES (1, 'one'), (2, 'two')")
        .await
        .unwrap();

    let tables = pool.introspect().await.unwrap();
    let table = find(&tables, "yb_nokey");
    let columns: Vec<&str> = table.columns.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(columns, ["a", "b"], "no implicit key column is exposed");
    assert_eq!(detect_row_identity(table, pool.dialect()), None);

    let access = pool.backend_access(table);
    assert!(!access.can_mutate());
    assert_eq!(access.restriction, Some(Restriction::NoRowIdentity));

    let page = pool
        .fetch_page(&PageRequest {
            schema: Some("public".into()),
            table: "yb_nokey".into(),
            limit: 5,
            offset: 0,
            sort: Some(("a".into(), SortDir::Asc)),
            filter: None,
            extra_key_column: None,
        })
        .await
        .unwrap();
    assert_eq!(page.rows.len(), 2, "still browsable");

    with_ddl(&pool, &["DROP TABLE yb_nokey CASCADE".to_string()]).await;
    pool.close().await;
}

#[tokio::test]
async fn yugabyte_materialized_view_browses_but_refuses_writes() {
    let Some(url) = test_url() else { return };
    let pool = DbPool::open_postgres(&url).await.unwrap();
    let (readings, _) = fresh_fixture(&pool, "matview").await;
    let view = format!("{readings}_daily");

    // Yugabyte has real materialized views, which CockroachDB does not — so
    // the `relkind = 'm'` half of introspection (FRE-41) is exercised here and
    // nowhere else in this milestone.
    with_ddl(
        &pool,
        &[
            format!("DROP MATERIALIZED VIEW IF EXISTS {view}"),
            format!(
                "CREATE MATERIALIZED VIEW {view} AS
                 SELECT sensor_id, avg(temperature) AS avg_temp
                 FROM {readings} GROUP BY sensor_id"
            ),
        ],
    )
    .await;

    let tables = pool.introspect().await.unwrap();
    let meta = find(&tables, &view);
    assert_eq!(meta.kind, TableKind::MaterializedView);
    assert!(
        meta.columns.iter().any(|c| c.name == "avg_temp"),
        "matview columns come from pg_attribute, not information_schema"
    );
    assert_eq!(detect_row_identity(meta, pool.dialect()), None);

    let access = pool.backend_access(meta);
    assert!(!access.can_mutate());
    assert_eq!(access.restriction, Some(Restriction::MaterializedView));

    let page = pool
        .fetch_page(&PageRequest {
            schema: Some("public".into()),
            table: view.clone(),
            limit: 10,
            offset: 0,
            sort: Some(("sensor_id".into(), SortDir::Asc)),
            filter: None,
            extra_key_column: None,
        })
        .await
        .unwrap();
    assert_eq!(page.rows.len(), 2, "the matview should have rows");

    with_ddl(&pool, &[format!("DROP MATERIALIZED VIEW {view}")]).await;
    pool.close().await;
}

#[tokio::test]
async fn yugabyte_extension_objects_are_attributed_like_any_postgres() {
    let Some(url) = test_url() else { return };
    let pool = DbPool::open_postgres(&url).await.unwrap();

    // The FRE-88 machinery runs off `pg_depend`, which is real here — unlike
    // CockroachDB, where it is empty and the equivalent objects had to be found
    // another way. `pg_buffercache` installs a view into `public`, which is the
    // per-object case (as opposed to the per-schema one), and is the same
    // extension `tests/db_timescale.rs` uses to reach it.
    //
    // Not `pg_stat_statements`, which this image preinstalls: on Yugabyte it
    // puts its views in `pg_catalog`, so introspection excludes them before
    // attribution ever runs and the case would go untested while appearing to
    // pass. Created rather than assumed for the same reason — extensions are
    // per-database, and the test database is made empty.
    with_ddl(
        &pool,
        &[
            "CREATE EXTENSION IF NOT EXISTS pg_buffercache".to_string(),
            "DROP TABLE IF EXISTS yb_ext_neighbour".to_string(),
            "CREATE TABLE yb_ext_neighbour (id int PRIMARY KEY)".to_string(),
        ],
    )
    .await;

    let tables = pool.introspect().await.unwrap();
    let view = find(&tables, "pg_buffercache");
    assert_eq!(
        view.internal,
        Some(Internal::Extension("pg_buffercache".into())),
        "{view:?}"
    );
    // ...while this test's own table in that same schema stays the user's.
    assert_eq!(find(&tables, "yb_ext_neighbour").internal, None);

    // Nothing here should be attributed to the *engine* — `Internal::System`
    // is CockroachDB's reserved-catalog case, and Yugabyte has no equivalent:
    // it keeps everything in `pg_catalog`, which introspection excludes.
    assert!(
        !tables.iter().any(|t| t.internal == Some(Internal::System)),
        "Yugabyte exposes no reserved catalog schema of its own"
    );

    with_ddl(
        &pool,
        &[
            "DROP TABLE yb_ext_neighbour".to_string(),
            "DROP EXTENSION pg_buffercache".to_string(),
        ],
    )
    .await;
    pool.close().await;
}

#[tokio::test]
async fn yugabyte_script_dml_rolls_back_but_its_ddl_does_not() {
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

    // The same gap CockroachDB has, reached differently: Yugabyte commits each
    // schema change as it executes rather than auto-committing the transaction
    // around it. Pinned here as well as there because it is what makes this a
    // *capability* rather than one engine's quirk — FRE-146 narrows what hubro
    // claims, and must not simply declare `transactions: false`, since the DML
    // half below demonstrably still works.
    with_ddl(
        &pool,
        &[
            "DROP TABLE IF EXISTS yb_script_ddl CASCADE".to_string(),
            "DROP TABLE IF EXISTS yb_script_dml CASCADE".to_string(),
            "CREATE TABLE yb_script_dml (id int PRIMARY KEY)".to_string(),
        ],
    )
    .await;

    let guard = ddl_lock().lock().await;
    let sql = "INSERT INTO yb_script_dml VALUES (1); \
               CREATE TABLE yb_script_ddl (id int PRIMARY KEY); \
               SELECT * FROM missing_relation";
    let statements = split_statements(sql, pool.dialect());
    let error = run_script(&pool, pool.backend_capabilities(), &statements, |_| {})
        .await
        .expect_err("the final statement names no relation");
    assert_eq!(
        error.rollback,
        Rollback::ExceptSchemaChanges,
        "the rollback covered the DML but not the schema change, and hubro must say so"
    );
    drop(guard);

    let survivors = pool
        .query(
            "SELECT count(*) FROM information_schema.tables \
             WHERE table_name = 'yb_script_ddl'",
        )
        .await
        .unwrap();
    assert_eq!(
        integer(&survivors.rows[0][0]),
        1,
        "known gap: the schema change is committed as it executes"
    );

    // ...while the write in the same script really is undone. This is the half
    // that keeps `transactions: true` honest.
    let rows = pool
        .query("SELECT count(*) FROM yb_script_dml")
        .await
        .unwrap();
    assert_eq!(
        integer(&rows.rows[0][0]),
        0,
        "DML must still roll back with the batch"
    );

    with_ddl(
        &pool,
        &[
            "DROP TABLE IF EXISTS yb_script_ddl CASCADE".to_string(),
            "DROP TABLE yb_script_dml CASCADE".to_string(),
        ],
    )
    .await;
    pool.close().await;
}

#[tokio::test]
async fn yugabyte_refuses_concurrent_ddl_without_corrupting_anything() {
    let Some(url) = test_url() else { return };
    let pool = DbPool::open_postgres(&url).await.unwrap();

    // The finding with real teeth, pinned so it is a known property rather than
    // an intermittent mystery. Two schema changes in flight at once conflict on
    // the cluster-wide catalog version, and Yugabyte cannot replay a statement
    // that is not first in its batch — so one of them fails.
    //
    // What matters for hubro is the shape of the failure, which is what this
    // asserts: a plain statement error carrying the engine's own explanation,
    // with the winner fully applied. hubro does reach this — run slots are
    // per connection, so two open connections can issue DDL at once, and a
    // cancelled Postgres statement keeps running server-side, so
    // cancel-then-rerun can overlap two on one connection — but far less often
    // than a test suite does, which is why this file has a DDL lock at all.
    with_ddl(
        &pool,
        &[
            "DROP TABLE IF EXISTS yb_race_a CASCADE".to_string(),
            "DROP TABLE IF EXISTS yb_race_b CASCADE".to_string(),
        ],
    )
    .await;

    // The lock is held *across* the deliberate race: these two must contend
    // with each other and nothing else, or this test becomes the reason its
    // siblings fail — which is precisely the failure mode it exists to
    // document.
    let guard = ddl_lock().lock().await;
    let (a, b) = tokio::join!(
        pool.query("CREATE TABLE yb_race_a (id int PRIMARY KEY)"),
        pool.query("CREATE TABLE yb_race_b (id int PRIMARY KEY)"),
    );
    drop(guard);

    // Observed as a conflict every time it was tried, but not asserted as
    // "exactly one fails": that is a timing claim about a distributed cluster,
    // and a test that depends on losing a race is a test that goes flaky on a
    // faster machine. What must hold either way is that a loser fails
    // *cleanly*, which is the part hubro's behaviour actually rests on.
    //
    // The loser is also where hubro's transient classification (FRE-147) is
    // pinned against a *real* server-sent SQLSTATE rather than a hand-built
    // error: this is the one place in the suite that reliably makes a live
    // engine raise `40001`. The retry that classification feeds is on the
    // catalog reads, not here — a DDL conflict is reported, never re-run — but
    // if the code stopped being recognised, this is what would notice.
    for outcome in [&a, &b] {
        if let Err(error) = outcome {
            let message = error.message().to_lowercase();
            assert!(
                message.contains("concurrent update") || message.contains("catalog"),
                "a DDL conflict should name itself, got: {message}"
            );
            assert!(
                error.is_transient(),
                "a serialization failure should classify as transient, got: {error:?}"
            );
        }
    }

    // Whichever succeeded is a complete, usable table — the conflict is a
    // refusal, not a partial apply.
    let tables = pool.introspect().await.unwrap();
    for (name, outcome) in [("yb_race_a", &a), ("yb_race_b", &b)] {
        if outcome.is_ok() {
            let table = find(&tables, name);
            let pk: Vec<&str> = table
                .primary_key()
                .iter()
                .map(|c| c.name.as_str())
                .collect();
            assert_eq!(pk, ["id"], "{name} should be fully formed");
        }
    }

    with_ddl(
        &pool,
        &[
            "DROP TABLE IF EXISTS yb_race_a CASCADE".to_string(),
            "DROP TABLE IF EXISTS yb_race_b CASCADE".to_string(),
        ],
    )
    .await;
    pool.close().await;
}
