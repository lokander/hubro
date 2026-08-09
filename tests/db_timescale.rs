//! TimescaleDB verification (FRE-88). Timescale is an extension on stock
//! PostgreSQL, so the Postgres backend drives it unchanged — these tests pin
//! the parts that are *not* stock: hypertables (one logical table spread over
//! many chunk tables), continuous aggregates, and the extension's own schemas.
//!
//! Needs a running server (Docker only, per CLAUDE.md) and is skipped unless
//! `HUBRO_TIMESCALE_TEST_URL` is set, e.g.:
//!
//! ```sh
//! docker run -d --name hubro-timescale-test -p 5434:5432 \
//!   -e POSTGRES_PASSWORD=hubro timescale/timescaledb:latest-pg17
//! docker exec hubro-timescale-test psql -U postgres -c 'CREATE DATABASE demo'
//! HUBRO_TIMESCALE_TEST_URL=postgres://postgres:hubro@localhost:5434/demo cargo test
//! ```
//!
//! The rest of the Postgres suite also passes against this container — point
//! `HUBRO_PG_TEST_URL` at it to re-run it there.

use hubro::db::{
    apply_staged, detect_row_identity, DbPool, Filter, Internal, PageRequest, Restriction,
    RowIdentity, RowLocator, SortDir, StagedChange, TableKind, TableMeta, Value,
};

fn test_url() -> Option<String> {
    match std::env::var("HUBRO_TIMESCALE_TEST_URL") {
        Ok(url) => Some(url),
        Err(_) => {
            eprintln!("skipping timescale test: HUBRO_TIMESCALE_TEST_URL not set");
            None
        }
    }
}

/// A hypertable with a composite `(time, sensor_id)` primary key, a foreign
/// key out to a plain table, and enough days of data to be spread over
/// several chunks. Suffixed per test so the tests in this binary can run
/// concurrently without fighting over one fixture.
async fn fresh_hypertable(pool: &DbPool, suffix: &str) -> (String, String) {
    let readings = format!("readings_{suffix}");
    let sensors = format!("sensors_{suffix}");
    for sql in [
        format!("DROP TABLE IF EXISTS {readings} CASCADE"),
        format!("DROP TABLE IF EXISTS {sensors} CASCADE"),
        format!("CREATE TABLE {sensors} (id int PRIMARY KEY, name text NOT NULL)"),
        format!("INSERT INTO {sensors} (id, name) VALUES (1, 'alpha'), (2, 'beta')"),
        format!(
            "CREATE TABLE {readings} (
                time        timestamptz NOT NULL,
                sensor_id   int NOT NULL REFERENCES {sensors}(id),
                temperature double precision,
                note        text,
                PRIMARY KEY (time, sensor_id)
            )"
        ),
        format!("SELECT create_hypertable('{readings}', by_range('time', INTERVAL '1 day'))"),
        format!(
            "INSERT INTO {readings} (time, sensor_id, temperature)
             SELECT ts, s.id, extract(epoch from ts) / 1e9
             FROM generate_series(
                 '2026-01-01'::timestamptz, '2026-01-05'::timestamptz, INTERVAL '6 hours'
             ) ts
             CROSS JOIN {sensors} s"
        ),
    ] {
        pool.query(&sql).await.unwrap();
    }
    (readings, sensors)
}

/// The rendered text of a cell. Timestamps arrive as text in a fixed ISO-ish
/// layout, so comparing the strings compares the instants.
fn text(value: &Value) -> &str {
    match value {
        Value::Text(text) => text,
        other => panic!("expected a text cell, got {other:?}"),
    }
}

fn find<'a>(tables: &'a [TableMeta], name: &str) -> &'a TableMeta {
    tables
        .iter()
        .find(|t| t.name == name && t.schema.as_deref() == Some("public"))
        .unwrap_or_else(|| panic!("{name} missing from introspection"))
}

#[tokio::test]
async fn timescale_hypertable_introspects_as_an_ordinary_table() {
    let Some(url) = test_url() else { return };
    let pool = DbPool::open_postgres(&url).await.unwrap();
    let (readings, sensors) = fresh_hypertable(&pool, "intro").await;

    let tables = pool.introspect().await.unwrap();
    let table = find(&tables, &readings);

    // A hypertable is a plain table with a trigger, and hubro should see
    // exactly that — no special kind, no chunk parentage leaking out.
    assert_eq!(table.kind, TableKind::Table);
    assert_eq!(table.internal, None);
    let columns: Vec<&str> = table.columns.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(columns, ["time", "sensor_id", "temperature", "note"]);

    let pk: Vec<&str> = table
        .primary_key()
        .iter()
        .map(|c| c.name.as_str())
        .collect();
    assert_eq!(pk, ["time", "sensor_id"]);

    let fk = table.foreign_keys.first().expect("fk to the sensors table");
    assert_eq!(fk.columns, ["sensor_id"]);
    assert_eq!(fk.referenced_table, sensors);

    // Timescale creates a descending time index of its own on every
    // hypertable; it is a real index and should be reported like one.
    assert!(
        table
            .indexes
            .iter()
            .any(|i| i.columns == ["time"] && !i.unique),
        "expected Timescale's implicit time index, got {:?}",
        table.indexes
    );
}

#[tokio::test]
async fn timescale_internal_objects_are_attributed_to_the_extension() {
    let Some(url) = test_url() else { return };
    let pool = DbPool::open_postgres(&url).await.unwrap();
    fresh_hypertable(&pool, "marked").await;

    // This fixture's own chunks by name, so the assertion below doesn't
    // lean on chunks another test in this binary happened to leave behind.
    let mine = pool
        .query(
            "SELECT chunk_name FROM timescaledb_information.chunks \
             WHERE hypertable_name = 'readings_marked'",
        )
        .await
        .unwrap();
    let mine: Vec<String> = mine
        .rows
        .iter()
        .map(|row| text(&row[0]).to_string())
        .collect();
    assert!(mine.len() >= 4, "expected several chunks, got {mine:?}");

    let tables = pool.introspect().await.unwrap();

    // Every chunk is attributed, which is what lets the sidebar hide them
    // (FRE-88).
    let chunks: Vec<&TableMeta> = tables
        .iter()
        .filter(|t| t.schema.as_deref() == Some("_timescaledb_internal"))
        .collect();
    for name in &mine {
        assert!(
            chunks.iter().any(|c| &c.name == name),
            "chunk {name} missing from introspection"
        );
    }
    for chunk in &chunks {
        assert_eq!(
            chunk.internal,
            Some(Internal::Extension("timescaledb".into())),
            "{chunk:?}"
        );
    }

    // The catalog and the user-facing information views are attributed too.
    for schema in [
        "_timescaledb_catalog",
        "_timescaledb_config",
        "timescaledb_information",
    ] {
        let found = tables
            .iter()
            .filter(|t| t.schema.as_deref() == Some(schema))
            .collect::<Vec<_>>();
        assert!(!found.is_empty(), "no objects found in {schema}");
        for table in found {
            assert_eq!(
                table.internal,
                Some(Internal::Extension("timescaledb".into())),
                "{table:?}"
            );
        }
    }

    // Installing an extension must not make the user's own schema the
    // extension's: nothing in `public` is attributed to timescaledb, even
    // though `pg_extension.extnamespace` for it *is* `public`. (Objects in
    // `public` can still be internal for their own reasons — an extension's
    // own table, a child partition — which is a different claim.)
    for table in tables
        .iter()
        .filter(|t| t.schema.as_deref() == Some("public"))
    {
        assert_ne!(
            table.internal,
            Some(Internal::Extension("timescaledb".into())),
            "{table:?}"
        );
    }
}

#[tokio::test]
async fn timescale_hypertable_pages_sorts_and_filters_across_chunks() {
    let Some(url) = test_url() else { return };
    let pool = DbPool::open_postgres(&url).await.unwrap();
    let (readings, _) = fresh_hypertable(&pool, "page").await;

    // 17 timestamps × 2 sensors, so a filtered count has to reach across
    // every chunk rather than stopping at the first.
    let request = PageRequest {
        schema: Some("public".into()),
        table: readings.clone(),
        limit: 3,
        offset: 0,
        sort: Some(("time".into(), SortDir::Desc)),
        filter: Some(Filter::equals("sensor_id", "2")),
        extra_key_column: None,
    };
    assert_eq!(pool.count_rows(&request).await.unwrap(), 17);

    let page = pool.fetch_page(&request).await.unwrap();
    assert_eq!(page.rows.len(), 3);
    let times: Vec<&str> = page.rows.iter().map(|row| text(&row[0])).collect();
    let mut sorted = times.clone();
    sorted.sort_by(|a, b| b.cmp(a));
    assert_eq!(times, sorted, "rows should come back newest first");

    // Offsets keep walking the same ordering rather than restarting per chunk.
    let next = PageRequest {
        offset: 3,
        ..request
    };
    let next = pool.fetch_page(&next).await.unwrap();
    assert!(text(&next.rows[0][0]) < times[2]);
}

#[tokio::test]
async fn timescale_hypertable_rows_edit_through_the_composite_key() {
    let Some(url) = test_url() else { return };
    let pool = DbPool::open_postgres(&url).await.unwrap();
    let (readings, _) = fresh_hypertable(&pool, "edit").await;

    let tables = pool.introspect().await.unwrap();
    let table = find(&tables, &readings);
    let identity = detect_row_identity(table, pool.dialect()).expect("composite pk");
    assert_eq!(
        identity,
        RowIdentity::PrimaryKey {
            columns: vec!["time".into(), "sensor_id".into()]
        }
    );

    let access = pool.backend_access(table);
    let key = || RowLocator {
        identity_values: vec![
            Value::Text("2026-01-01 00:00:00+00".into()),
            Value::Integer(1),
        ],
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

    // An insert outside every existing chunk's range makes Timescale create a
    // new chunk behind the scenes; the backend should neither know nor care.
    let far_future = RowLocator {
        identity_values: vec![
            Value::Text("2027-06-01 12:00:00+00".into()),
            Value::Integer(1),
        ],
    };
    let counts = apply_staged(
        &pool,
        &access,
        table,
        &identity,
        &[StagedChange::Insert {
            columns: vec!["time".into(), "sensor_id".into(), "temperature".into()],
            values: vec![
                Value::Text("2027-06-01 12:00:00+00".into()),
                Value::Integer(1),
                Value::Real(9.5),
            ],
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
            locator: far_future,
        }],
    )
    .await
    .unwrap();
    assert_eq!(counts.deleted_rows, 1);
}

#[tokio::test]
async fn timescale_continuous_aggregate_is_a_readable_read_only_view() {
    let Some(url) = test_url() else { return };
    let pool = DbPool::open_postgres(&url).await.unwrap();
    let (readings, _) = fresh_hypertable(&pool, "cagg").await;
    let agg = format!("{readings}_daily");

    pool.query(&format!("DROP MATERIALIZED VIEW IF EXISTS {agg}"))
        .await
        .unwrap();
    pool.query(&format!(
        "CREATE MATERIALIZED VIEW {agg} WITH (timescaledb.continuous) AS
         SELECT time_bucket('1 day', time) AS bucket, sensor_id, avg(temperature) AS avg_temp
         FROM {readings} GROUP BY bucket, sensor_id WITH NO DATA"
    ))
    .await
    .unwrap();
    pool.query(&format!(
        "CALL refresh_continuous_aggregate('{agg}', NULL, NULL)"
    ))
    .await
    .unwrap();

    let tables = pool.introspect().await.unwrap();
    let view = find(&tables, &agg);

    // Timescale implements a continuous aggregate as a view over a hidden
    // materialization hypertable, so it arrives as a plain view — and a view
    // has no addressable rows, which is the answer that keeps editing off it.
    assert_eq!(view.kind, TableKind::View);
    assert_eq!(view.internal, None);
    assert_eq!(detect_row_identity(view, pool.dialect()), None);
    assert!(!pool.backend_access(view).can_mutate());

    let page = pool
        .fetch_page(&PageRequest {
            schema: Some("public".into()),
            table: agg.clone(),
            limit: 10,
            offset: 0,
            sort: Some(("bucket".into(), SortDir::Asc)),
            filter: None,
            extra_key_column: None,
        })
        .await
        .unwrap();
    assert!(!page.rows.is_empty(), "the aggregate should have rows");

    pool.query(&format!("DROP MATERIALIZED VIEW {agg}"))
        .await
        .unwrap();
}

#[tokio::test]
async fn timescale_hypertable_without_a_primary_key_refuses_editing() {
    let Some(url) = test_url() else { return };
    let pool = DbPool::open_postgres(&url).await.unwrap();

    // Timescale requires any unique constraint to include the partitioning
    // column, so plenty of real hypertables simply have no key — and the
    // ones that do have a composite one. There is no rowid analogue to fall
    // back on, and a `ctid` fallback would be actively wrong here: rows move
    // between chunks under compression, so a ctid captured a moment ago can
    // address a different row. Browsing and the script tab still work; only
    // editing is refused, with a reason.
    pool.query("DROP TABLE IF EXISTS readings_nokey CASCADE")
        .await
        .unwrap();
    pool.query(
        "CREATE TABLE readings_nokey (
            time timestamptz NOT NULL, sensor_id int NOT NULL, temperature double precision
        )",
    )
    .await
    .unwrap();
    pool.query("SELECT create_hypertable('readings_nokey', by_range('time', INTERVAL '1 day'))")
        .await
        .unwrap();
    pool.query(
        "INSERT INTO readings_nokey \
         SELECT ts, 1, 20.0 \
         FROM generate_series('2026-01-01'::timestamptz,'2026-01-04'::timestamptz, INTERVAL '6 hours') ts",
    )
    .await
    .unwrap();

    let tables = pool.introspect().await.unwrap();
    let table = find(&tables, "readings_nokey");
    assert_eq!(detect_row_identity(table, pool.dialect()), None);

    let access = pool.backend_access(table);
    assert!(!access.can_mutate());
    assert_eq!(access.restriction, Some(Restriction::NoRowIdentity));

    // Still browsable, which is the half that has to keep working.
    let page = pool
        .fetch_page(&PageRequest {
            schema: Some("public".into()),
            table: "readings_nokey".into(),
            limit: 5,
            offset: 0,
            sort: Some(("time".into(), SortDir::Asc)),
            filter: None,
            extra_key_column: None,
        })
        .await
        .unwrap();
    assert_eq!(page.rows.len(), 5);

    pool.query("DROP TABLE readings_nokey CASCADE")
        .await
        .unwrap();
}

#[tokio::test]
async fn timescale_objects_carry_the_engines_own_vocabulary() {
    let Some(url) = test_url() else { return };
    let pool = DbPool::open_postgres(&url).await.unwrap();
    let (readings, sensors) = fresh_hypertable(&pool, "label").await;
    let agg = format!("{readings}_daily");

    pool.query(&format!("DROP MATERIALIZED VIEW IF EXISTS {agg}"))
        .await
        .unwrap();
    pool.query(&format!(
        "CREATE MATERIALIZED VIEW {agg} WITH (timescaledb.continuous) AS
         SELECT time_bucket('1 day', time) AS bucket, sensor_id, avg(temperature) AS avg_temp
         FROM {readings} GROUP BY bucket, sensor_id WITH NO DATA"
    ))
    .await
    .unwrap();

    let tables = pool.introspect().await.unwrap();

    // A hypertable is still a table and a continuous aggregate is still a
    // view — the label refines the kind rather than replacing it, so both
    // remain true at once.
    let hypertable = find(&tables, &readings);
    assert_eq!(hypertable.kind, TableKind::Table);
    assert_eq!(hypertable.kind_label.as_deref(), Some("hypertable"));

    let cagg = find(&tables, &agg);
    assert_eq!(cagg.kind, TableKind::View);
    assert_eq!(cagg.kind_label.as_deref(), Some("continuous aggregate"));

    // An ordinary table in the same database gets no label at all.
    assert_eq!(find(&tables, &sensors).kind_label, None);

    pool.query(&format!("DROP MATERIALIZED VIEW {agg}"))
        .await
        .unwrap();
}

#[tokio::test]
async fn an_extensions_table_in_an_ordinary_schema_is_still_internal() {
    let Some(url) = test_url() else { return };
    let pool = DbPool::open_postgres(&url).await.unwrap();

    // The case the schema-level rule alone would miss: `pg_buffercache`
    // installs its view into `public`, exactly as PostGIS does with
    // `spatial_ref_sys`. Attribution is per object as well as per schema
    // (FRE-88).
    pool.query("CREATE EXTENSION IF NOT EXISTS pg_buffercache")
        .await
        .unwrap();
    // This test's own table in the same schema, so the contrast below rests
    // on a table this test created rather than on whatever a sibling test
    // happened to leave behind — the tests in this binary run concurrently
    // against one database, and each must also pass run on its own.
    pool.query("DROP TABLE IF EXISTS ext_neighbour")
        .await
        .unwrap();
    pool.query("CREATE TABLE ext_neighbour (id int PRIMARY KEY)")
        .await
        .unwrap();

    let tables = pool.introspect().await.unwrap();
    assert_eq!(
        find(&tables, "pg_buffercache").internal,
        Some(Internal::Extension("pg_buffercache".into()))
    );
    // ...while the user's own table in that same schema stays the user's.
    assert_eq!(find(&tables, "ext_neighbour").internal, None);

    pool.query("DROP TABLE ext_neighbour").await.unwrap();
    pool.query("DROP EXTENSION pg_buffercache").await.unwrap();
}
