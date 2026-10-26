//! YugabyteDB verification. Yugabyte runs the *real* PostgreSQL query
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
//!  2. **Introspection can fail transiently — since fixed.**
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
//!     honestly.** The same behaviour CockroachDB has, for a
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
//! DDL and leaves the browsable metadata untouched. Neither the
//! access method nor the distribution is anything hubro records, so nothing
//! had to change to accommodate them.
//!
//! A table declared without a primary key has no user-visible key at all — no
//! `ctid`, and no implicit `rowid` of the kind CockroachDB adds — so it is
//! read-only, exactly as on stock Postgres.
//!
//! Storage statistics are half stock and half not, which is why they
//! get a test of their own here. `ANALYZE` writes `pg_class.reltuples` exactly
//! as PostgreSQL does, so the row estimate needs no allowance. But
//! `pg_total_relation_size` answers 0 for a table that has just been written —
//! Yugabyte populates its size accounting out of band and it arrives later.
//! Nothing in the backend is engine-specific about this: a zero size is dropped
//! as unaccounted on every backend, because nothing occupies literally nothing,
//! so "0 B" beside sixteen rows never reaches the screen. It is only the shared
//! suite that has to guard its size assertions, and only those.

mod common;

use common::pg_family::{
    assert_pages_sorts_and_filters, assert_rows_edit_through_key,
    assert_script_ddl_escapes_rollback, find, integer, DdlEscape, Expected, KeyedEdits,
    ReadingsKey, SensorFixture,
};

use hubro::db::Reading;
use std::sync::OnceLock;

use hubro::db::{
    detect_row_identity, split_statements, DbPool, DdlObject, DdlSource, Filter, Internal,
    PageRequest, PgFlavor, Restriction, RowCount, SortDir, TableKind, Value,
};
use tokio::sync::Mutex;

fn test_url() -> Option<String> {
    common::engine_url("HUBRO_YUGABYTE_TEST_URL")
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
    // Schema changes run under the lock; the rows load outside it, being
    // ordinary writes that Yugabyte runs concurrently without trouble.
    SensorFixture {
        prefix: "yb_",
        key: ReadingsKey::IdAndSensor,
        ddl_lock: Some(ddl_lock()),
    }
    .create(pool, suffix)
    .await
}

/// Runs schema changes that aren't part of [`fresh_fixture`], under the lock.
async fn with_ddl(pool: &DbPool, statements: &[String]) {
    let _guard = ddl_lock().lock().await;
    for sql in statements {
        pool.query_unbounded(sql).await.unwrap();
    }
}

#[tokio::test]
async fn yugabyte_is_not_mistaken_for_stock_postgres() {
    let Some(url) = test_url() else { return };
    let pool = DbPool::open_postgres(&url).await.unwrap();

    // Yugabyte's `version()` *leads* with `PostgreSQL 15.12-…`, because the
    // query layer genuinely is PostgreSQL 15 — so a detector that matched the
    // claimed Postgres version before looking for the engine's own name would
    // file this as stock. Capabilities branch on the answer: filed as stock,
    // Yugabyte would claim its schema changes roll back, which they do not.
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

    // The sharding attributes Yugabyte's indexes were expected to carry do
    // exist — they just live in the DDL rather than in `pg_index`, which is
    // byte-identical to stock Postgres here. `pg_get_indexdef` renders them,
    // so Show DDL reports the distribution the user actually has instead of a
    // B-tree that was never created. Native output, so this is the server's own
    // text.
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
        limit: 3,
        sort: Some(("id".into(), SortDir::Desc)),
        filter: Some(Filter::equals("sensor_id", "2")),
        ..common::page(readings.clone())
    };
    let (page, next) = assert_pages_sorts_and_filters(&pool, &request, Expected::Exactly(8)).await;
    // Rows are distributed by hash, so the storage order is arbitrary; an
    // ORDER BY has to impose the ordering rather than reflect it.
    let ids: Vec<i64> = page.rows.iter().map(|row| integer(&row[0])).collect();
    assert_eq!(ids, [8, 7, 6]);
    assert_eq!(integer(&next.rows[0][0]), 5);

    pool.close().await;
}

#[tokio::test]
async fn yugabyte_rows_edit_through_the_composite_key() {
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
    pool.query_unbounded("INSERT INTO yb_nokey (a, b) VALUES (1, 'one'), (2, 'two')")
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
        .fetch_page(
            Reading::UNMARKED,
            &PageRequest {
                schema: Some("public".into()),
                limit: 5,
                sort: Some(("a".into(), SortDir::Asc)),
                ..common::page("yb_nokey")
            },
        )
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
    // the `relkind = 'm'` half of introspection is exercised here and
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
        .fetch_page(
            Reading::UNMARKED,
            &PageRequest {
                schema: Some("public".into()),
                limit: 10,
                sort: Some(("sensor_id".into(), SortDir::Asc)),
                ..common::page(view.clone())
            },
        )
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

    // Internal-object marking runs off `pg_depend`, which is real here — unlike
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

    // The same gap CockroachDB has, reached differently: Yugabyte commits each
    // schema change as it executes rather than auto-committing the transaction
    // around it. Pinned here as well as there because it is what makes this a
    // *capability* rather than one engine's quirk — the warning that DDL
    // escaped a rollback narrows what hubro claims, and must not simply declare
    // `transactions: false`, since the DML half below demonstrably still works.
    assert_script_ddl_escapes_rollback(
        &pool,
        DdlEscape {
            prefix: "yb_",
            ddl_lock: Some(ddl_lock()),
            // ...while the write in the same script really is undone. This is
            // the half that keeps `transactions: true` honest.
            rows_left: (0, "DML must still roll back with the batch"),
            table_left: "known gap: the schema change is committed as it executes",
        },
    )
    .await;
    pool.close().await;
}

#[tokio::test]
async fn a_forced_serialization_failure_classifies_as_transient() {
    let Some(url) = test_url() else { return };
    let pool = DbPool::open_postgres(&url).await.unwrap();

    // `40001` raised on demand rather than by losing a race: a REPEATABLE
    // READ transaction takes its snapshot, a second session commits an update
    // to the same row, and the transaction's own update of that row is then
    // refused. The order is enforced by the awaits, so it does not depend on
    // how fast the cluster is. This is the classification the catalog reads'
    // one retry depends on, and Yugabyte is the engine that made it necessary
    // (the header's finding 2).
    with_ddl(
        &pool,
        &[
            "DROP TABLE IF EXISTS yb_serial CASCADE".to_string(),
            "CREATE TABLE yb_serial (id int PRIMARY KEY, v int)".to_string(),
        ],
    )
    .await;
    pool.execute("INSERT INTO yb_serial VALUES (1, 0)")
        .await
        .unwrap();

    let mut tx = pool.begin_script_tx().await.unwrap();
    tx.execute("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ")
        .await
        .unwrap();
    tx.query_capped("SELECT v FROM yb_serial WHERE id = 1", 1)
        .await
        .unwrap();
    pool.execute("UPDATE yb_serial SET v = 1 WHERE id = 1")
        .await
        .unwrap();
    let refused = tx
        .execute("UPDATE yb_serial SET v = 2 WHERE id = 1")
        .await
        .expect_err("a REPEATABLE READ update of a row changed since its snapshot must be refused");
    let _ = tx.rollback().await;

    assert!(
        refused.is_transient(),
        "a server-sent 40001 must classify as transient, got: {refused:?}"
    );

    with_ddl(&pool, &["DROP TABLE yb_serial CASCADE".to_string()]).await;
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
        pool.query_unbounded("CREATE TABLE yb_race_a (id int PRIMARY KEY)"),
        pool.query_unbounded("CREATE TABLE yb_race_b (id int PRIMARY KEY)"),
    );
    drop(guard);

    // Observed as a conflict every time it was tried, but not asserted as
    // "exactly one fails": that is a timing claim about a distributed cluster,
    // and a test that depends on losing a race is a test that goes flaky on a
    // faster machine. What must hold either way is that a loser fails
    // *cleanly*, which is the part hubro's behaviour actually rests on.
    //
    // The loser's error is also checked for transient classification. It is
    // not where that classification is *pinned*, though: everything in this
    // loop sits behind a race, and a run where neither statement loses checks
    // none of it and passes. (Measured on the development machine: one of the
    // two lost in 10 runs out of 10. That is an observation about this box,
    // not a property of the cluster.)
    // `a_forced_serialization_failure_classifies_as_transient` raises `40001`
    // with no race at all, and is what would notice the code going
    // unrecognised.
    for outcome in [&a, &b] {
        if let Err(error) = outcome {
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

#[tokio::test]
async fn yugabyte_row_estimate_arrives_but_its_size_accounting_lags() {
    let Some(url) = test_url() else { return };
    let pool = DbPool::open_postgres(&url).await.unwrap();
    // The fixture seeds sixteen readings (eight per sensor).
    let (readings, sensors) = fresh_fixture(&pool, "stats").await;
    // ANALYZE runs under the DDL lock, not because it changes the schema but
    // because Yugabyte serialises it against the catalog the other tests'
    // fixtures are writing: run loose, it fails a sibling's fixture INSERT
    // with "could not serialize access due to concurrent update", which names
    // neither test and looks like a hubro bug (finding 1 in the header).
    with_ddl(&pool, &[format!("ANALYZE {readings}")]).await;

    let tables = pool.introspect().await.unwrap();
    let meta = find(&tables, &readings).clone();

    // The row half is ordinary: ANALYZE writes `reltuples` exactly as stock
    // Postgres does, so the shared suite's estimate assertions hold here
    // unguarded.
    let stats = pool.fetch_table_stats(&meta).await.unwrap();
    assert_eq!(stats.rows, Some(RowCount::Estimated(16)));

    // The size half is not. Yugabyte's `pg_total_relation_size` answers 0 for
    // a table it was handed rows a moment ago — its storage accounting is
    // populated out of band and arrives later. A viewer that rendered that
    // literally would show "0 B" beside sixteen rows, so a zero size is
    // dropped as unaccounted rather than reported. That is what is
    // pinned here: never a zero, whichever side of the lag this run lands on.
    assert_ne!(
        stats.bytes,
        Some(0),
        "a zero size must never survive as a measurement"
    );
    assert!(
        stats.bytes.is_none_or(|b| b > 0),
        "any size reported must be a real one: {:?}",
        stats.bytes
    );

    // And the exact count is unaffected by any of it.
    assert_eq!(
        pool.count_table_rows(Reading::UNMARKED, &meta)
            .await
            .unwrap(),
        16
    );

    let _guard = ddl_lock().lock().await;
    pool.query_unbounded(&format!("DROP TABLE IF EXISTS {readings} CASCADE"))
        .await
        .unwrap();
    pool.query_unbounded(&format!("DROP TABLE IF EXISTS {sensors} CASCADE"))
        .await
        .unwrap();
    pool.close().await;
}

#[tokio::test]
async fn yugabyte_reports_a_transaction_only_when_a_script_leaves_one_open() {
    // Ending a run asks the server whether the script left a transaction
    // open — the transaction status every Postgres-wire server sends. An
    // ordinary script must come back with nothing rolled back (a server that
    // reported a transaction while idle would put the rolled-back note on
    // every run), and one that does leave a transaction open must say so.
    let Some(url) = test_url() else { return };
    let pool = DbPool::open_postgres(&url).await.unwrap();
    let caps = pool.backend_capabilities();
    let run = |sql: &str| {
        let statements = hubro::db::split_statements(sql, pool.dialect());
        let pool = &pool;
        async move { hubro::db::run_script(pool, caps, &statements, |_| {}).await }
    };
    let nothing = Ok(hubro::db::ScriptDone::default());
    assert_eq!(run("SELECT 1").await, nothing);
    assert_eq!(run("SELECT 1; SELECT 2").await, nothing);
    let left_open = run("BEGIN; SELECT 1").await;
    assert!(
        matches!(
            left_open,
            Ok(hubro::db::ScriptDone {
                rolled_back: Some(_),
                ..
            })
        ),
        "{left_open:?}"
    );
    assert_eq!(run("SELECT 1").await, nothing, "the next run starts clean");
    pool.close().await;
}

/// A read the row cap cut short that has written — through a function the
/// `SELECT` calls — keeps every write, though YugabyteDB never says it has
/// written: measured, its `pg_stat_activity.backend_xid` stayed empty while
/// the function inserted rows. Read as "has written nothing", that answer
/// cancelled the rest of the read and lost the writes, so hubro does not ask
/// this engine and reads the rest to its end.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_capped_read_that_writes_keeps_its_writes_though_yugabyte_cannot_say_it_wrote() {
    let Some(url) = test_url() else { return };
    let pool = DbPool::open_postgres(&url).await.unwrap();
    // Held across the run, not only the setup: the statement calls a
    // function, which reads the catalog, and a sibling's DDL meanwhile fails
    // it with MISMATCHED_SCHEMA — a transient error hubro reports as such,
    // and noise here.
    let _ddl = ddl_lock().lock().await;
    {
        for statement in [
            "DROP FUNCTION IF EXISTS yb_capped_bump(integer)",
            "DROP TABLE IF EXISTS yb_capped_side",
            "CREATE TABLE yb_capped_side (a integer)",
            // Rows of 2 KB, so the result outgrows the socket buffers and the
            // statement cannot finish before most of it has been read.
            "CREATE FUNCTION yb_capped_bump(i integer) RETURNS text LANGUAGE sql \
             AS 'INSERT INTO yb_capped_side VALUES (i) RETURNING repeat(''x'', 2000)'",
        ] {
            pool.execute(statement).await.unwrap();
        }
    }
    let read = "SELECT yb_capped_bump(g) FROM generate_series(1, 20000) g";
    let mut expected = 0;
    // As the last statement the rest is read after the run, so its outcome
    // is not claimed; before another statement it is read in the run.
    for (script, rest) in [
        (read.to_string(), hubro::db::RestOfRead::Unconfirmed),
        (format!("{read}; SELECT 1"), hubro::db::RestOfRead::Ran),
    ] {
        let statements = split_statements(&script, pool.dialect());
        let mut results = Vec::new();
        let (outcome, pending) = hubro::db::run_script_watching_rest(
            &pool,
            pool.backend_capabilities(),
            &statements,
            |r| results.push(r),
        )
        .await;
        outcome.unwrap_or_else(|e| panic!("{script}: {e:?}"));
        assert!(results[0].truncated, "{script}");
        assert_eq!(results[0].rest, rest, "{script}");
        // Only a rest read after the run has an outcome still to come, and
        // here it finishes.
        match pending {
            Some(pending) => {
                assert_eq!(rest, hubro::db::RestOfRead::Unconfirmed, "{script}");
                assert_eq!(
                    pending.outcome().await,
                    hubro::db::LateRest::Finished,
                    "{script}"
                );
            }
            None => assert_eq!(rest, hubro::db::RestOfRead::Ran, "{script}"),
        }
        expected += 20_000;
        // The last read is read in the background before its transaction
        // commits.
        let mut kept = Value::Null;
        for _ in 0..300 {
            kept = pool
                .query_unbounded("SELECT count(*) FROM yb_capped_side")
                .await
                .unwrap()
                .rows[0][0]
                .clone();
            if kept == Value::Integer(expected) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        assert_eq!(kept, Value::Integer(expected), "{script}: writes lost");
    }
    pool.execute("DROP FUNCTION yb_capped_bump(integer)")
        .await
        .unwrap();
    pool.execute("DROP TABLE yb_capped_side").await.unwrap();
    pool.close().await;
}

/// A last read the row cap cut short, whose rest fails after the run has
/// ended, is not reported as having run: its function inserts a row per call
/// and divides by zero at call 15,000, past the cap. The rest is read in the
/// background, the error reaches only that read, and the statement's rows are
/// all undone — measured, 0 of them kept while the run reported the rest
/// `Ran`. Now the run says `Unconfirmed`, and the rows are still gone, which is
/// what that word warns of.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_capped_read_failing_after_the_run_is_not_claimed_to_have_run() {
    let Some(url) = test_url() else { return };
    let pool = DbPool::open_postgres(&url).await.unwrap();
    // Held across the run, not only the setup: the statement calls a
    // function, which reads the catalog, and a sibling's DDL meanwhile fails
    // it with MISMATCHED_SCHEMA — a transient error hubro reports as such,
    // and noise here.
    let _ddl = ddl_lock().lock().await;
    {
        for statement in [
            "DROP FUNCTION IF EXISTS yb_capped_fail(integer)",
            "DROP TABLE IF EXISTS yb_capped_fail_side",
            "CREATE TABLE yb_capped_fail_side (a integer)",
            "CREATE FUNCTION yb_capped_fail(i integer) RETURNS text LANGUAGE sql \
             AS 'INSERT INTO yb_capped_fail_side VALUES (i / (i - 15000)) \
                 RETURNING repeat(''x'', 2000)'",
        ] {
            pool.execute(statement).await.unwrap();
        }
    }
    let statements = split_statements(
        "SELECT yb_capped_fail(g) FROM generate_series(1, 20000) g",
        pool.dialect(),
    );
    let mut results = Vec::new();
    let (outcome, pending) =
        hubro::db::run_script_watching_rest(&pool, pool.backend_capabilities(), &statements, |r| {
            results.push((r.truncated, r.rest))
        })
        .await;
    assert_eq!(outcome, Ok(hubro::db::ScriptDone::default()));
    assert_eq!(results, [(true, hubro::db::RestOfRead::Unconfirmed)]);
    // The run reported before the rest ended; the rest's outcome arrives
    // after, and is the server's error.
    let late = pending
        .expect("the rest is read after the run")
        .outcome()
        .await;
    assert!(
        matches!(&late, hubro::db::LateRest::Failed(error) if error.contains("division by zero")),
        "{late:?}"
    );
    // The background read meets the error and the statement is undone. Its
    // rows are uncommitted until then, so the count is read only once no
    // session is running the statement any more. That moment is the end of
    // the rest: hubro closes the connection after the background read
    // (`postgres::drain_then_close`), and a closed session leaves
    // `pg_stat_activity`, so zero sessions means the statement has ended,
    // committed or undone, and the count is final.
    let mut running = Value::Null;
    for _ in 0..300 {
        running = pool
            .query_unbounded(
                "SELECT count(*) FROM pg_stat_activity \
                 WHERE query LIKE 'SELECT yb_capped_fail(g)%' AND pid <> pg_backend_pid()",
            )
            .await
            .unwrap()
            .rows[0][0]
            .clone();
        if running == Value::Integer(0) {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert_eq!(running, Value::Integer(0), "the rest never finished");
    let kept = pool
        .query_unbounded("SELECT count(*) FROM yb_capped_fail_side")
        .await
        .unwrap()
        .rows[0][0]
        .clone();
    assert_eq!(
        kept,
        Value::Integer(0),
        "the failed statement's rows were kept"
    );
    pool.execute("DROP FUNCTION yb_capped_fail(integer)")
        .await
        .unwrap();
    pool.execute("DROP TABLE yb_capped_fail_side")
        .await
        .unwrap();
    pool.close().await;
}
