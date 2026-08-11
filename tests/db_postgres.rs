//! Postgres integration tests. They need a running server (Docker only, per
//! CLAUDE.md) and are skipped unless `HUBRO_PG_TEST_URL` is set, e.g.:
//!
//! ```sh
//! docker run -d --name hubro-pg-test -e POSTGRES_PASSWORD=testpass \
//!   -e POSTGRES_USER=tester -e POSTGRES_DB=demo -p 5433:5432 postgres:17-alpine
//! HUBRO_PG_TEST_URL=postgres://tester:testpass@localhost:5433/demo cargo test
//! ```
//!
//! The suite runs in a database of its own, created by `common::pg_test_url`
//! (FRE-127): cargo runs the Postgres suites in parallel and they use the same
//! fixture names, so sharing one database meant dropping each other's tables.

mod common;

use hubro::db::{
    detect_row_identity, explain_statement, needs_confirmation, run_script, script_refusal,
    url_with_password, Capabilities, DbError, DbPool, Dialect, Filter, Internal, PageRequest,
    PgFlavor, PlanDisplay, Restriction, Rollback, RowCount, RowLocator, SortDir, TableKind,
    TypeDetail, TypeRef, Value, PREVIEW_BYTES, QUERY_CELL_CAP,
};

async fn test_url() -> Option<String> {
    common::pg_test_url().await
}

async fn fresh_fixture(pool: &DbPool, table: &str) {
    pool.query(&format!("DROP TABLE IF EXISTS {table}"))
        .await
        .unwrap();
    pool.query(&format!(
        "CREATE TABLE {table} (
            id serial PRIMARY KEY,
            name text NOT NULL,
            weight real,
            data bytea
        )"
    ))
    .await
    .unwrap();
    pool.query(&format!(
        "INSERT INTO {table} (name, weight, data) VALUES
            ('apple', 1.5, '\\x0102'),
            ('banana', NULL, NULL),
            ('a_c', 2.5, NULL),
            ('avocado', 0.5, NULL)"
    ))
    .await
    .unwrap();
}

#[tokio::test]
async fn stock_postgres_is_detected_as_stock_postgres() {
    let Some(url) = test_url().await else { return };
    let pool = DbPool::open_postgres(&url).await.unwrap();

    // The safety claim behind flavor detection (FRE-90), asserted from the
    // side CI can actually reach: an engine-specific branch must never fire
    // against real PostgreSQL. `tests/db_cockroach.rs` pins the other
    // direction, but that binary needs a Cockroach container and CI has none —
    // so without this, a detection change that swept stock Postgres into an
    // engine branch would pass everything CI runs.
    assert_eq!(pool.pg_flavor(), Some(PgFlavor::Postgres));

    pool.close().await;
}

#[tokio::test]
async fn postgres_introspection_lists_public_tables_and_columns() {
    let Some(url) = test_url().await else { return };
    let pool = DbPool::open_postgres(&url).await.unwrap();
    fresh_fixture(&pool, "fruits_intro").await;

    let tables = pool.introspect().await.unwrap();
    let fruits = tables.iter().find(|t| t.name == "fruits_intro").unwrap();
    assert_eq!(fruits.kind, TableKind::Table);
    let names: Vec<&str> = fruits.columns.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, ["id", "name", "weight", "data"]);
    let name = fruits.columns.iter().find(|c| c.name == "name").unwrap();
    assert!(!name.nullable);
    let weight = fruits.columns.iter().find(|c| c.name == "weight").unwrap();
    assert!(weight.nullable);
    assert_eq!(weight.type_name, "real");

    pool.close().await;
}

#[tokio::test]
async fn postgres_paging_sorting_filtering_and_values_work() {
    let Some(url) = test_url().await else { return };
    let pool = DbPool::open_postgres(&url).await.unwrap();
    fresh_fixture(&pool, "fruits_page").await;

    let mut request = PageRequest {
        schema: None,
        table: "fruits_page".into(),
        limit: 2,
        offset: 0,
        sort: Some(("name".into(), SortDir::Asc)),
        filter: None,
        extra_key_column: None,
    };
    assert_eq!(pool.count_rows(&request).await.unwrap(), 4);
    let page = pool.fetch_page(&request).await.unwrap();
    assert_eq!(page.rows.len(), 2);
    assert_eq!(page.rows[0][1], Value::Text("a_c".into()));
    assert_eq!(page.rows[1][1], Value::Text("apple".into()));

    // Contains filter with an underscore matches literally, not as a wildcard.
    request.filter = Some(Filter::contains("name", "a_"));
    request.limit = 10;
    let filtered = pool.fetch_page(&request).await.unwrap();
    assert_eq!(filtered.rows.len(), 1);
    assert_eq!(filtered.rows[0][1], Value::Text("a_c".into()));

    // Equals filter on a numeric column via the ::text cast.
    request.filter = Some(Filter::equals("id", "1"));
    let by_id = pool.fetch_page(&request).await.unwrap();
    assert_eq!(by_id.rows.len(), 1);
    // serial/int4, real, text, bytea, and NULL all decode.
    assert_eq!(by_id.rows[0][0], Value::Integer(1));
    assert_eq!(by_id.rows[0][1], Value::Text("apple".into()));
    assert_eq!(by_id.rows[0][2], Value::Real(1.5));
    assert_eq!(by_id.rows[0][3], Value::Blob(vec![1, 2]));

    pool.close().await;
}

#[tokio::test]
async fn same_named_pk_and_fk_constraints_do_not_confuse_pk_detection() {
    let Some(url) = test_url().await else { return };
    let pool = DbPool::open_postgres(&url).await.unwrap();
    for sql in [
        "DROP SCHEMA IF EXISTS pkbug CASCADE",
        "CREATE SCHEMA pkbug",
        "CREATE TABLE pkbug.t1 (id integer, CONSTRAINT samename PRIMARY KEY (id))",
        "CREATE TABLE pkbug.t2 (
            own_id integer,
            ref integer,
            CONSTRAINT t2_pkey PRIMARY KEY (own_id),
            CONSTRAINT samename FOREIGN KEY (ref) REFERENCES pkbug.t1 (id)
        )",
    ] {
        pool.query(sql).await.unwrap();
    }

    let tables = pool.introspect().await.unwrap();
    let t2 = tables
        .iter()
        .find(|t| t.schema.as_deref() == Some("pkbug") && t.name == "t2")
        .unwrap();
    // The FK named like t1's PK must not mark t2.ref as a PK column…
    let ref_col = t2.columns.iter().find(|c| c.name == "ref").unwrap();
    assert_eq!(ref_col.primary_key_position, None);
    // …nor duplicate any column rows.
    assert_eq!(t2.columns.len(), 2);
    let pk: Vec<&str> = t2.primary_key().iter().map(|c| c.name.as_str()).collect();
    assert_eq!(pk, ["own_id"]);

    pool.close().await;
}

#[tokio::test]
async fn postgres_rich_types_render_correctly() {
    let Some(url) = test_url().await else { return };
    let pool = DbPool::open_postgres(&url).await.unwrap();
    for sql in [
        "DROP TABLE IF EXISTS rich_types",
        "DROP TYPE IF EXISTS mood_rich_types",
        "CREATE TYPE mood_rich_types AS ENUM ('sad', 'ok', 'happy')",
        "CREATE TABLE rich_types (
            id serial PRIMARY KEY,
            flag boolean,
            ts timestamp,
            ts_frac timestamp,
            tstz timestamptz,
            d date,
            t time,
            t_frac time,
            ttz timetz,
            iv interval,
            iv_neg interval,
            num numeric(30, 10),
            uid uuid,
            j json,
            jb jsonb,
            mood mood_rich_types,
            tags text[],
            nums int4[],
            bigs int8[],
            smalls int2[],
            floats float8[],
            flags bool[],
            uids uuid[],
            decs numeric[],
            r int4range
        )",
        "INSERT INTO rich_types (
            flag, ts, ts_frac, tstz, d, t, t_frac, ttz, iv, iv_neg, num, uid,
            j, jb, mood, tags, nums, bigs, smalls, floats, flags, uids, decs, r
        ) VALUES (
            true,
            '2024-03-05 07:08:09',
            '2024-03-05 07:08:09.123456',
            '2024-03-05 07:08:09+02',
            '2024-03-05',
            '07:08:09',
            '07:08:09.5',
            '07:08:09+02',
            '1 mon 2 days 03:04:05',
            '-3 days',
            '123456789012345678.0987654321',
            'A0EEBC99-9C0B-4EF8-BB6D-6BB9BD380A11',
            '{\"a\": 1, \"b\": [true, null]}',
            '{\"z\": \"txt\", \"y\": 2.5}',
            'happy',
            ARRAY['x', 'y', NULL],
            ARRAY[1, 2, 3],
            ARRAY[9223372036854775807],
            ARRAY[7::int2],
            ARRAY[1.5, 2.25],
            ARRAY[true, false],
            ARRAY['A0EEBC99-9C0B-4EF8-BB6D-6BB9BD380A11'::uuid],
            ARRAY[1.50, 2.5]::numeric[],
            int4range(1, 5)
        )",
    ] {
        pool.query(sql).await.unwrap();
    }

    let result = pool
        .query("SELECT * FROM rich_types ORDER BY id")
        .await
        .unwrap();
    assert_eq!(result.rows.len(), 1);
    let row = &result.rows[0];
    let col = |name: &str| {
        let idx = result
            .columns
            .iter()
            .position(|c| c.name == name)
            .unwrap_or_else(|| panic!("no column {name}"));
        &row[idx]
    };

    // Booleans render as text — clearer in a viewer than 0/1.
    assert_eq!(*col("flag"), Value::Text("true".into()));
    // Date/time: fractional seconds only when present, no trailing zeros.
    assert_eq!(*col("ts"), Value::Text("2024-03-05 07:08:09".into()));
    assert_eq!(
        *col("ts_frac"),
        Value::Text("2024-03-05 07:08:09.123456".into())
    );
    // timestamptz normalizes to UTC with an explicit offset ('+02' input).
    assert_eq!(
        *col("tstz"),
        Value::Text("2024-03-05 05:08:09+00:00".into())
    );
    assert_eq!(*col("d"), Value::Text("2024-03-05".into()));
    assert_eq!(*col("t"), Value::Text("07:08:09".into()));
    assert_eq!(*col("t_frac"), Value::Text("07:08:09.5".into()));
    // timetz keeps its stored offset.
    assert_eq!(*col("ttz"), Value::Text("07:08:09+02:00".into()));
    assert_eq!(*col("iv"), Value::Text("1 mon 2 days 03:04:05".into()));
    assert_eq!(*col("iv_neg"), Value::Text("-3 days".into()));
    // numeric with 28 significant digits survives exactly — proof the
    // value never went through f64 (which holds ~15-17). rust_decimal
    // caps at 28-29 significant digits; beyond that the decode fails and
    // the cell degrades to the <numeric> marker (covered in
    // postgres_undecodable_cells_degrade_without_erroring_the_page).
    assert_eq!(
        *col("num"),
        Value::Text("123456789012345678.0987654321".into())
    );
    // uuid is hyphenated lowercase regardless of input case.
    assert_eq!(
        *col("uid"),
        Value::Text("a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11".into())
    );
    // json/jsonb re-serialize compactly (whitespace from the input is not
    // preserved); jsonb additionally re-orders keys.
    assert_eq!(*col("j"), Value::Text("{\"a\":1,\"b\":[true,null]}".into()));
    assert_eq!(*col("jb"), Value::Text("{\"y\":2.5,\"z\":\"txt\"}".into()));
    // Enum columns render their label, not a fallback marker.
    assert_eq!(*col("mood"), Value::Text("happy".into()));
    // Arrays render as Postgres-style literals; NULL elements spelled out.
    assert_eq!(*col("tags"), Value::Text("{x,y,NULL}".into()));
    assert_eq!(*col("nums"), Value::Text("{1,2,3}".into()));
    assert_eq!(*col("bigs"), Value::Text("{9223372036854775807}".into()));
    assert_eq!(*col("smalls"), Value::Text("{7}".into()));
    assert_eq!(*col("floats"), Value::Text("{1.5,2.25}".into()));
    assert_eq!(*col("flags"), Value::Text("{true,false}".into()));
    assert_eq!(
        *col("uids"),
        Value::Text("{a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11}".into())
    );
    assert_eq!(*col("decs"), Value::Text("{1.50,2.5}".into()));
    // Exotic types keep the graceful marker fallback instead of erroring.
    assert_eq!(*col("r"), Value::Text("<int4range>".into()));

    pool.close().await;
}

#[tokio::test]
async fn postgres_undecodable_cells_degrade_without_erroring_the_page() {
    let Some(url) = test_url().await else { return };
    let pool = DbPool::open_postgres(&url).await.unwrap();
    for sql in [
        "DROP TABLE IF EXISTS degrade_cells",
        "CREATE TABLE degrade_cells (
            id serial PRIMARY KEY,
            nan_num numeric,
            big_num numeric,
            matrix int4[],
            ts_inf timestamp,
            ts_neg_inf timestamp,
            tstz_inf timestamptz,
            tstz_neg_inf timestamptz,
            d_inf date,
            d_neg_inf date,
            ok_text text
        )",
        "INSERT INTO degrade_cells (
            nan_num, big_num, matrix, ts_inf, ts_neg_inf, tstz_inf,
            tstz_neg_inf, d_inf, d_neg_inf, ok_text
        ) VALUES (
            'NaN',
            123456789012345678901234567890123456789,
            '{{1,2},{3,4}}',
            'infinity',
            '-infinity',
            'infinity',
            '-infinity',
            'infinity',
            '-infinity',
            'still here'
        )",
    ] {
        pool.query(sql).await.unwrap();
    }

    // The whole row (page) must render despite the hostile cells — no Err,
    // and certainly no panic.
    let result = pool
        .query("SELECT * FROM degrade_cells ORDER BY id")
        .await
        .unwrap();
    assert_eq!(result.rows.len(), 1);
    let row = &result.rows[0];
    let col = |name: &str| {
        let idx = result
            .columns
            .iter()
            .position(|c| c.name == name)
            .unwrap_or_else(|| panic!("no column {name}"));
        &row[idx]
    };

    // rust_decimal can represent neither NaN nor 39 significant digits;
    // both cells degrade to the marker instead of erroring the page.
    assert_eq!(*col("nan_num"), Value::Text("<numeric>".into()));
    assert_eq!(*col("big_num"), Value::Text("<numeric>".into()));
    // sqlx only decodes one-dimensional arrays; a 2-D array degrades.
    assert_eq!(*col("matrix"), Value::Text("<int4[]>".into()));
    // Infinite timestamps/dates would panic inside chrono if they reached
    // it; the wire-format special case renders them like psql does.
    assert_eq!(*col("ts_inf"), Value::Text("infinity".into()));
    assert_eq!(*col("ts_neg_inf"), Value::Text("-infinity".into()));
    assert_eq!(*col("tstz_inf"), Value::Text("infinity".into()));
    assert_eq!(*col("tstz_neg_inf"), Value::Text("-infinity".into()));
    assert_eq!(*col("d_inf"), Value::Text("infinity".into()));
    assert_eq!(*col("d_neg_inf"), Value::Text("-infinity".into()));
    // Healthy cells in the same row are unaffected.
    assert_eq!(*col("id"), Value::Integer(1));
    assert_eq!(*col("ok_text"), Value::Text("still here".into()));

    pool.close().await;
}

#[tokio::test]
async fn postgres_bad_password_is_an_authentication_error() {
    let Some(url) = test_url().await else { return };
    let wrong = url_with_password(&url, "definitely-wrong-password").unwrap();
    let err = DbPool::open_postgres(&wrong)
        .await
        .err()
        .expect("wrong password must fail");
    match err {
        DbError::Connect(msg) => assert!(
            msg.contains("authentication failed"),
            "unexpected message: {msg}"
        ),
        other => panic!("expected Connect error, got {other:?}"),
    }
}

#[tokio::test]
async fn postgres_multi_schema_introspection_has_parity_metadata() {
    let Some(url) = test_url().await else { return };
    let pool = DbPool::open_postgres(&url).await.unwrap();
    for sql in [
        "DROP SCHEMA IF EXISTS warehouse CASCADE",
        "CREATE SCHEMA warehouse",
        // Same-named table in two schemas: attribution must not bleed.
        "DROP TABLE IF EXISTS wh_probe",
        "CREATE TABLE wh_probe (public_only integer)",
        "CREATE TABLE warehouse.wh_probe (warehouse_only text)",
        "CREATE TABLE warehouse.locations (
            region text NOT NULL,
            slot integer NOT NULL,
            label text,
            PRIMARY KEY (region, slot)
        )",
        "CREATE TABLE warehouse.stock (
            id serial PRIMARY KEY,
            region text NOT NULL,
            slot integer NOT NULL,
            note text,
            FOREIGN KEY (region, slot) REFERENCES warehouse.locations (region, slot)
        )",
        "CREATE UNIQUE INDEX stock_note_unique ON warehouse.stock (note)",
        "CREATE INDEX stock_expr_idx ON warehouse.stock (lower(note))",
        "CREATE VIEW warehouse.stock_notes AS SELECT note FROM warehouse.stock",
        "INSERT INTO warehouse.locations VALUES ('eu', 1, 'shelf A')",
        "INSERT INTO warehouse.stock (region, slot, note) VALUES ('eu', 1, 'first')",
        // Materialized view created after the insert so it snapshots the row;
        // a unique index on it must NOT make it look editable (FRE-41).
        "CREATE MATERIALIZED VIEW warehouse.stock_mv AS \
         SELECT id, note FROM warehouse.stock",
        "CREATE UNIQUE INDEX stock_mv_id ON warehouse.stock_mv (id)",
        // No PK and no usable unique index: nothing addresses one row.
        "CREATE TABLE warehouse.keyless (a integer, b text)",
    ] {
        pool.query(sql).await.unwrap();
    }

    let tables = pool.introspect().await.unwrap();

    // Multi-schema: same-named tables in different schemas stay separate,
    // each with its own columns.
    let public_probe = tables
        .iter()
        .find(|t| t.schema.as_deref() == Some("public") && t.name == "wh_probe")
        .unwrap();
    assert_eq!(public_probe.columns.len(), 1);
    assert_eq!(public_probe.columns[0].name, "public_only");
    let warehouse_probe = tables
        .iter()
        .find(|t| t.schema.as_deref() == Some("warehouse") && t.name == "wh_probe")
        .unwrap();
    assert_eq!(warehouse_probe.columns[0].name, "warehouse_only");
    let locations = tables
        .iter()
        .find(|t| t.schema.as_deref() == Some("warehouse") && t.name == "locations")
        .unwrap();
    // Composite PK in declaration order.
    let pk: Vec<&str> = locations
        .primary_key()
        .iter()
        .map(|c| c.name.as_str())
        .collect();
    assert_eq!(pk, ["region", "slot"]);

    let stock = tables
        .iter()
        .find(|t| t.schema.as_deref() == Some("warehouse") && t.name == "stock")
        .unwrap();
    // Unique and expression indexes.
    assert!(stock
        .indexes
        .iter()
        .any(|i| i.name == "stock_note_unique" && i.unique && i.columns == ["note"]));
    assert!(stock
        .indexes
        .iter()
        .any(|i| i.name == "stock_expr_idx" && !i.unique && i.columns == ["<expr>"]));
    // Multi-column FK with ordering and referenced schema.
    let fk = stock
        .foreign_keys
        .iter()
        .find(|fk| fk.referenced_table == "locations")
        .unwrap();
    assert_eq!(fk.columns, ["region", "slot"]);
    assert_eq!(fk.referenced_schema.as_deref(), Some("warehouse"));
    assert_eq!(
        fk.referenced_columns,
        [Some("region".to_string()), Some("slot".to_string())]
    );

    let view = tables
        .iter()
        .find(|t| t.schema.as_deref() == Some("warehouse") && t.name == "stock_notes")
        .unwrap();
    assert_eq!(view.kind, TableKind::View);

    // Materialized view (FRE-41): introspected with its own kind and columns
    // (from pg_catalog, since information_schema omits matviews), and read-only
    // — its unique index must not yield a row identity.
    // Exactly once: the pg_catalog UNION must not double-list it alongside an
    // information_schema row.
    assert_eq!(
        tables
            .iter()
            .filter(|t| t.schema.as_deref() == Some("warehouse") && t.name == "stock_mv")
            .count(),
        1
    );
    let matview = tables
        .iter()
        .find(|t| t.schema.as_deref() == Some("warehouse") && t.name == "stock_mv")
        .unwrap();
    assert_eq!(matview.kind, TableKind::MaterializedView);
    let mv_columns: Vec<&str> = matview.columns.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(mv_columns, ["id", "note"]);
    assert!(
        detect_row_identity(matview, Dialect::Postgres).is_none(),
        "a matview must be read-only even with a unique index"
    );

    // Capabilities (FRE-87). Postgres declares the full set at the
    // connection level; the per-object resolution is what differs, and each
    // narrowing states its own reason.
    assert_eq!(pool.backend_capabilities(), Capabilities::FULL);
    let stock_access = pool.backend_access(stock);
    assert_eq!(stock_access.caps, Capabilities::FULL);
    assert_eq!(stock_access.restriction, None);
    assert!(stock_access.identity.is_some());

    let view_access = pool.backend_access(view);
    assert!(!view_access.can_mutate());
    assert_eq!(view_access.restriction, Some(Restriction::View));

    let mv_access = pool.backend_access(matview);
    assert!(!mv_access.can_mutate());
    assert_eq!(mv_access.restriction, Some(Restriction::MaterializedView));
    // Reduced, not disabled: browsing and paging a matview still work.
    assert!(mv_access.caps.read_query);
    assert!(mv_access.caps.offset_paging);

    let keyless = tables
        .iter()
        .find(|t| t.schema.as_deref() == Some("warehouse") && t.name == "keyless")
        .unwrap();
    let keyless_access = pool.backend_access(keyless);
    assert!(!keyless_access.can_mutate());
    assert_eq!(keyless_access.restriction, Some(Restriction::NoRowIdentity));
    assert_eq!(keyless_access.identity, None);

    // Browsing the matview's data works through the normal paged read path.
    let mv_request = PageRequest {
        schema: Some("warehouse".into()),
        table: "stock_mv".into(),
        limit: 10,
        offset: 0,
        sort: None,
        filter: None,
        extra_key_column: None,
    };
    assert_eq!(pool.count_rows(&mv_request).await.unwrap(), 1);
    let mv_page = pool.fetch_page(&mv_request).await.unwrap();
    assert_eq!(mv_page.rows[0][1], Value::Text("first".into()));

    // Schema-qualified paging works end to end.
    let request = PageRequest {
        schema: Some("warehouse".into()),
        table: "stock".into(),
        limit: 10,
        offset: 0,
        sort: None,
        filter: None,
        extra_key_column: None,
    };
    assert_eq!(pool.count_rows(&request).await.unwrap(), 1);
    let page = pool.fetch_page(&request).await.unwrap();
    assert_eq!(page.rows[0][3], Value::Text("first".into()));

    pool.close().await;
}

#[tokio::test]
async fn finite_datetimes_beyond_chrono_range_degrade_to_markers() {
    let Some(url) = test_url().await else { return };
    let pool = DbPool::open_postgres(&url).await.unwrap();
    // Postgres accepts these; chrono caps at ~year 262143, and sqlx's
    // decode would panic on the overflow without the guard.
    let result = pool
        .query(
            "SELECT '5874000-01-01'::date AS far_date, \
                    '294000-01-01 00:00:00+00'::timestamptz AS far_ts, \
                    'ok' AS healthy",
        )
        .await
        .unwrap();
    assert_eq!(
        result.rows[0][0],
        Value::Text("<out of chrono range>".into())
    );
    assert_eq!(
        result.rows[0][1],
        Value::Text("<out of chrono range>".into())
    );
    assert_eq!(result.rows[0][2], Value::Text("ok".into()));
    pool.close().await;
}

// ---- Bounded-memory reads (FRE-33) ---------------------------------------

#[tokio::test]
async fn postgres_bounded_page_previews_large_text_and_bytea() {
    let Some(url) = test_url().await else { return };
    let pool = DbPool::open_postgres(&url).await.unwrap();
    pool.query("DROP TABLE IF EXISTS bounded_docs")
        .await
        .unwrap();
    pool.query(
        "CREATE TABLE bounded_docs (
            id integer PRIMARY KEY,
            small_note text,
            big_text text,
            payload bytea
        )",
    )
    .await
    .unwrap();
    // A 50 000-char text and a 40 000-byte bytea.
    pool.query(
        "INSERT INTO bounded_docs (id, small_note, big_text, payload) VALUES
            (1, 'hi', repeat('A', 50000), decode(repeat('00', 40000), 'hex')),
            (2, 'yo', 'short enough', decode('0102', 'hex'))",
    )
    .await
    .unwrap();

    let tables = pool.introspect().await.unwrap();
    let docs = tables
        .iter()
        .find(|t| t.name == "bounded_docs")
        .unwrap()
        .clone();
    let request = PageRequest {
        schema: docs.schema.clone(),
        table: "bounded_docs".into(),
        limit: 100,
        offset: 0,
        sort: Some(("id".into(), SortDir::Asc)),
        filter: None,
        extra_key_column: None,
    };

    let page = pool
        .fetch_page_bounded(&request, &docs.columns, &["id"])
        .await
        .unwrap();
    assert_eq!(page.result.columns.len(), 4, "length columns stripped");
    // big_text previewed with the real length; bytea previewed as size.
    let text_preview = page.previews[0][2].expect("big_text truncated");
    assert_eq!(text_preview.full_len, 50_000);
    assert!(!text_preview.binary);
    if let Value::Text(t) = &page.result.rows[0][2] {
        assert!(t.chars().count() <= PREVIEW_BYTES);
    } else {
        panic!("expected text preview");
    }
    let blob_preview = page.previews[0][3].expect("bytea truncated");
    assert_eq!(blob_preview.full_len, 40_000);
    assert!(blob_preview.binary);
    // Short values are complete.
    assert!(page.previews[1][2].is_none());
    assert!(page.previews[0][1].is_none());

    // fetch_cell returns the full text.
    let identity = detect_row_identity(&docs, pool.dialect()).unwrap();
    let locator = RowLocator {
        identity_values: vec![Value::Integer(1)],
    };
    let cell = pool
        .fetch_cell(&docs, &identity, &locator, "big_text")
        .await
        .unwrap();
    assert_eq!(cell.full_len, 50_000);
    assert!(!cell.capped);
    if let Value::Text(t) = &cell.value {
        assert_eq!(t.chars().count(), 50_000);
    } else {
        panic!("expected full text");
    }

    pool.query("DROP TABLE IF EXISTS bounded_docs")
        .await
        .unwrap();
    pool.close().await;
}

#[tokio::test]
async fn postgres_query_capped_stops_and_bounds_cells() {
    let Some(url) = test_url().await else { return };
    let pool = DbPool::open_postgres(&url).await.unwrap();

    // Row cap: 25 rows exist, ask for 10.
    let (result, truncated) = pool
        .query_capped(
            "SELECT g FROM generate_series(1, 25) AS g ORDER BY g",
            &[],
            10,
        )
        .await
        .unwrap();
    assert_eq!(result.rows.len(), 10);
    assert!(truncated);

    // Under the cap: no truncation.
    let (result, truncated) = pool
        .query_capped("SELECT g FROM generate_series(1, 5) AS g", &[], 100)
        .await
        .unwrap();
    assert_eq!(result.rows.len(), 5);
    assert!(!truncated);

    // Huge cell capped.
    let (result, _t) = pool
        .query_capped("SELECT repeat('Z', 200000) AS v", &[], 10)
        .await
        .unwrap();
    if let Value::Text(t) = &result.rows[0][0] {
        assert!(t.len() <= QUERY_CELL_CAP, "cell capped, got {}", t.len());
    } else {
        panic!("expected text");
    }

    pool.close().await;
}

/// A zero-row result still carries its column headers (FRE-138) — the same
/// contract the SQLite and SQL Server backends hold to.
#[tokio::test]
async fn postgres_empty_results_keep_their_column_headers() {
    let Some(url) = test_url().await else { return };
    let pool = DbPool::open_postgres(&url).await.unwrap();
    fresh_fixture(&pool, "fruits_empty").await;

    let sql = "SELECT id, name FROM fruits_empty WHERE false";
    let empty = pool.query(sql).await.unwrap();
    assert!(empty.rows.is_empty());
    let names: Vec<&str> = empty.columns.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, ["id", "name"]);

    let (empty, truncated) = pool.query_capped(sql, &[], 100).await.unwrap();
    assert!(empty.rows.is_empty());
    assert!(!truncated);
    let names: Vec<&str> = empty.columns.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, ["id", "name"]);

    // A statement with no result set has no header to recover, and must not
    // become an error because of it.
    pool.execute("DROP TABLE IF EXISTS pg_headerless")
        .await
        .unwrap();
    pool.execute("CREATE TABLE pg_headerless (x integer)")
        .await
        .unwrap();
    let (result, _) = pool
        .query_capped("DROP TABLE pg_headerless", &[], 100)
        .await
        .unwrap();
    assert!(result.rows.is_empty());
    assert!(result.columns.is_empty());

    pool.close().await;
}

/// Enum and array columns are reported by information_schema as the opaque
/// `USER-DEFINED` / `ARRAY`, so introspection resolves the real structure
/// from pg_catalog for the type-aware editors (FRE-71).
#[tokio::test]
async fn postgres_introspection_resolves_enum_variants_and_array_columns() {
    let Some(url) = test_url().await else { return };
    let pool = DbPool::open_postgres(&url).await.unwrap();
    pool.query("DROP TABLE IF EXISTS enum_intro").await.unwrap();
    pool.query("DROP TYPE IF EXISTS intro_mood").await.unwrap();
    pool.query("CREATE TYPE intro_mood AS ENUM ('sad', 'ok', 'happy')")
        .await
        .unwrap();
    pool.query(
        "CREATE TABLE enum_intro (
            id serial PRIMARY KEY,
            feeling intro_mood,
            tags text[],
            plain text
        )",
    )
    .await
    .unwrap();

    let tables = pool.introspect().await.unwrap();
    let table = tables.iter().find(|t| t.name == "enum_intro").unwrap();
    let col = |name: &str| {
        table
            .columns
            .iter()
            .find(|c| c.name == name)
            .unwrap_or_else(|| panic!("no column {name}"))
    };

    // Variants come back in declaration order, not alphabetical.
    assert_eq!(
        col("feeling").type_detail,
        TypeDetail::Enum {
            // Kept schema-qualified so the staged cast resolves regardless
            // of search_path.
            type_ref: TypeRef {
                schema: "public".into(),
                name: "intro_mood".into(),
            },
            variants: vec!["sad".into(), "ok".into(), "happy".into()],
        }
    );
    // Built-in array types live in pg_catalog, not the table's schema.
    assert_eq!(
        col("tags").type_detail,
        TypeDetail::Array {
            type_ref: TypeRef {
                schema: "pg_catalog".into(),
                name: "_text".into(),
            }
        }
    );
    // Ordinary columns are unaffected.
    assert_eq!(col("plain").type_detail, TypeDetail::Plain);
    assert_eq!(col("id").type_detail, TypeDetail::Plain);

    pool.query("DROP TABLE enum_intro").await.unwrap();
    pool.query("DROP TYPE intro_mood").await.unwrap();
    pool.close().await;
}

/// A case-sensitive (quoted) enum type in its own schema: the staged cast
/// must be quoted per identifier, or it resolves to a lowercased name that
/// doesn't exist. Regression test for the FRE-71 review.
#[tokio::test]
async fn postgres_quoted_camelcase_enum_saves_through_the_staged_cast() {
    let Some(url) = test_url().await else { return };
    let pool = DbPool::open_postgres(&url).await.unwrap();
    pool.query("DROP TABLE IF EXISTS camel_intro")
        .await
        .unwrap();
    pool.query("DROP SCHEMA IF EXISTS camel_ns CASCADE")
        .await
        .unwrap();
    pool.query("CREATE SCHEMA camel_ns").await.unwrap();
    pool.query(r#"CREATE TYPE camel_ns."Mood" AS ENUM ('sad', 'happy')"#)
        .await
        .unwrap();
    pool.query(r#"CREATE TABLE camel_intro (id int PRIMARY KEY, m camel_ns."Mood")"#)
        .await
        .unwrap();
    pool.query("INSERT INTO camel_intro VALUES (1, 'sad')")
        .await
        .unwrap();

    let tables = pool.introspect().await.unwrap();
    let table = tables.iter().find(|t| t.name == "camel_intro").unwrap();
    let column = table.columns.iter().find(|c| c.name == "m").unwrap();
    assert_eq!(
        column.type_detail,
        TypeDetail::Enum {
            type_ref: TypeRef {
                schema: "camel_ns".into(),
                name: "Mood".into(),
            },
            variants: vec!["sad".into(), "happy".into()],
        }
    );

    // The staged UPDATE has to survive the round trip, not just introspect.
    let identity = detect_row_identity(table, Dialect::Postgres).unwrap();
    let applied = hubro::db::apply_staged(
        &pool,
        &pool.backend_access(table),
        table,
        &identity,
        &[hubro::db::StagedChange::Update {
            locator: RowLocator {
                identity_values: vec![Value::Integer(1)],
            },
            column: "m".into(),
            value: Value::Text("happy".into()),
        }],
    )
    .await
    .unwrap();
    assert_eq!(applied.updated_rows, 1);
    let after = pool
        .query("SELECT m::text FROM camel_intro WHERE id = 1")
        .await
        .unwrap();
    assert_eq!(after.rows[0][0], Value::Text("happy".into()));

    pool.query("DROP TABLE camel_intro").await.unwrap();
    pool.query("DROP SCHEMA camel_ns CASCADE").await.unwrap();
    pool.close().await;
}

/// A `bit(n)` / `bit varying` column can actually be written (FRE-159).
///
/// The unit tests pin the SQL that is generated; only a live server can say
/// whether Postgres accepts it. That distinction is the whole reason this bug
/// existed: `staged.rs` documented, in prose, that skipping the cast was
/// correct because "assignment coercion handles text → bit(n)" — and it does
/// not. Nothing executed the claim, so it went unchallenged.
///
/// The `character(3)` half of that same sentence *is* true, so it is checked
/// here too rather than left as the surviving half of a claim that was wrong.
#[tokio::test]
async fn bit_and_char_columns_save_through_the_staged_cast() {
    let Some(url) = test_url().await else { return };
    let pool = DbPool::open_postgres(&url).await.unwrap();
    pool.query("DROP TABLE IF EXISTS bit_intro").await.unwrap();
    pool.query(
        "CREATE TABLE bit_intro (id int PRIMARY KEY, mask bit(4), flags bit varying(8), \
         code character(3))",
    )
    .await
    .unwrap();
    pool.query("INSERT INTO bit_intro VALUES (1, B'0000', B'0', 'aaa')")
        .await
        .unwrap();

    let tables = pool.introspect().await.unwrap();
    let table = tables.iter().find(|t| t.name == "bit_intro").unwrap();
    // The bare name is what makes `bit` ambiguous: the modifier is dropped, so
    // `::bit` would mean `bit(1)`.
    let mask = table.columns.iter().find(|c| c.name == "mask").unwrap();
    assert_eq!(mask.type_name, "bit");
    let flags = table.columns.iter().find(|c| c.name == "flags").unwrap();
    assert_eq!(flags.type_name, "bit varying");

    let identity = detect_row_identity(table, Dialect::Postgres).unwrap();
    let locator = RowLocator {
        identity_values: vec![Value::Integer(1)],
    };
    let applied = hubro::db::apply_staged(
        &pool,
        &pool.backend_access(table),
        table,
        &identity,
        &[
            hubro::db::StagedChange::Update {
                locator: locator.clone(),
                column: "mask".into(),
                value: Value::Text("1010".into()),
            },
            hubro::db::StagedChange::Update {
                locator: locator.clone(),
                column: "flags".into(),
                value: Value::Text("1101".into()),
            },
            hubro::db::StagedChange::Update {
                locator,
                column: "code".into(),
                value: Value::Text("xyz".into()),
            },
        ],
    )
    .await
    .expect("a bit column must be writable — an uncast text parameter is refused outright");
    // One row, not three edits: changes to the same row coalesce into a single
    // UPDATE, so all three casts are exercised by one statement.
    assert_eq!(applied.updated_rows, 1);

    let after = pool
        .query("SELECT mask::text, flags::text, code FROM bit_intro WHERE id = 1")
        .await
        .unwrap();
    assert_eq!(after.rows[0][0], Value::Text("1010".into()));
    assert_eq!(after.rows[0][1], Value::Text("1101".into()));
    // `character(3)` is blank-padded by the server; the point is that the
    // value arrived whole rather than truncated to one character, which is
    // what a `::character` cast would have done.
    assert_eq!(after.rows[0][2], Value::Text("xyz".into()));

    pool.query("DROP TABLE bit_intro").await.unwrap();
    pool.close().await;
}

/// Bit values are readable, and a `bit(n)` **primary key** round-trips.
///
/// A writable column nobody can read is not usable: sqlx has no built-in
/// decode for these OIDs, so before the `BIT`/`VARBIT` arm every bit cell
/// showed the `<bit>` marker. That is not only a display gap — the marker
/// becomes the row locator for a `bit(n)` key, so the UPDATE keys on `"<bit>"`
/// and the (now correct) cast turns it into `"<" is not a valid binary digit`.
/// Such a table was unsaveable no matter how right the cast was.
#[tokio::test]
async fn bit_values_are_readable_and_a_bit_key_round_trips() {
    let Some(url) = test_url().await else { return };
    let pool = DbPool::open_postgres(&url).await.unwrap();
    pool.query("DROP TABLE IF EXISTS bit_key").await.unwrap();
    pool.query("CREATE TABLE bit_key (m bit(4) PRIMARY KEY, label text, wide bit varying(64))")
        .await
        .unwrap();
    pool.query("INSERT INTO bit_key VALUES (B'1010', 'a', B'110011001100')")
        .await
        .unwrap();

    // Read: the value itself, not a marker.
    let read = pool.query("SELECT m, wide FROM bit_key").await.unwrap();
    assert_eq!(
        read.rows[0][0],
        Value::Text("1010".into()),
        "a bit column must read as its bits — the `<bit>` marker also becomes \
         the row locator, which makes a bit-keyed table unsaveable"
    );
    assert_eq!(read.rows[0][1], Value::Text("110011001100".into()));

    // Write, keyed by that same bit value as the grid would hand it back.
    let tables = pool.introspect().await.unwrap();
    let table = tables.iter().find(|t| t.name == "bit_key").unwrap();
    let identity = detect_row_identity(table, Dialect::Postgres).unwrap();
    let applied = hubro::db::apply_staged(
        &pool,
        &pool.backend_access(table),
        table,
        &identity,
        &[hubro::db::StagedChange::Update {
            locator: RowLocator {
                // Exactly what the read above produced — the point of the test.
                identity_values: vec![read.rows[0][0].clone()],
            },
            column: "label".into(),
            value: Value::Text("b".into()),
        }],
    )
    .await
    .expect("a bit(n) primary key must be usable as a row locator");
    assert_eq!(
        applied.updated_rows, 1,
        "the key matched no row, so the save silently changed nothing"
    );

    let after = pool
        .query("SELECT label FROM bit_key WHERE m = B'1010'")
        .await
        .unwrap();
    assert_eq!(after.rows[0][0], Value::Text("b".into()));

    pool.query("DROP TABLE bit_key").await.unwrap();
    pool.close().await;
}

#[tokio::test]
async fn partition_children_are_internal_but_their_parent_is_not() {
    let Some(url) = test_url().await else { return };
    let pool = DbPool::open_postgres(&url).await.unwrap();
    pool.query("DROP TABLE IF EXISTS parted_intro CASCADE")
        .await
        .unwrap();
    pool.query("CREATE TABLE parted_intro (id int, at date NOT NULL) PARTITION BY RANGE (at)")
        .await
        .unwrap();
    pool.query(
        "CREATE TABLE parted_intro_2026_01 PARTITION OF parted_intro \
         FOR VALUES FROM ('2026-01-01') TO ('2026-02-01')",
    )
    .await
    .unwrap();

    let tables = pool.introspect().await.unwrap();
    let find = |name: &str| {
        tables
            .iter()
            .find(|t| t.name == name && t.schema.as_deref() == Some("public"))
            .unwrap_or_else(|| panic!("{name} missing from introspection"))
    };

    // The parent is the table you browse, so it stays visible; the children
    // are the flooding problem, and are hidden by the same rule that hides
    // an extension's objects (FRE-88). Nothing here involves an extension.
    assert_eq!(find("parted_intro").internal, None);
    assert_eq!(
        find("parted_intro_2026_01").internal,
        Some(Internal::Partition)
    );

    pool.query("DROP TABLE parted_intro CASCADE").await.unwrap();
    pool.close().await;
}

/// Whether this server accounts for an object's size as soon as it is written
/// (FRE-118).
///
/// This binary is pointed at TimescaleDB, Citus and YugabyteDB as well as
/// stock Postgres, and the last of those answers `pg_total_relation_size` with
/// 0 for a table it was handed rows a moment ago — its size accounting arrives
/// later, out of band. Its *row* estimate is ordinary and needs no allowance,
/// so only the size claims are guarded. `tests/db_yugabyte.rs` pins that
/// engine's side of this directly.
///
/// The claims that hold everywhere — that a zero never surfaces as a
/// measurement, that a view reports no size — stay unguarded, which is where
/// most of the value is.
fn accounts_for_size_immediately(pool: &DbPool) -> bool {
    pool.pg_flavor() != Some(PgFlavor::Yugabyte)
}

/// Helper: the introspected metadata for one `public` table.
async fn meta_of(pool: &DbPool, name: &str) -> hubro::db::TableMeta {
    pool.introspect()
        .await
        .unwrap()
        .into_iter()
        .find(|t| t.name == name && t.schema.as_deref() == Some("public"))
        .unwrap_or_else(|| panic!("{name} missing from introspection"))
}

#[tokio::test]
async fn table_stats_estimate_rows_only_once_something_has_measured_them() {
    let Some(url) = test_url().await else { return };
    let pool = DbPool::open_postgres(&url).await.unwrap();
    fresh_fixture(&pool, "stats_intro").await;
    let meta = meta_of(&pool, "stats_intro").await;

    // Nothing has analyzed the table yet, so `pg_class.reltuples` is -1. The
    // whole point of FRE-118's absent case: a viewer that reported this as
    // "0 rows" would be stating something false about a table with four.
    let before = pool.fetch_table_stats(&meta).await.unwrap();
    assert_eq!(
        before.rows, None,
        "an unanalyzed table must report no estimate, not a zero"
    );
    // A size is either a real measurement or absent; a zero is neither, and
    // is dropped rather than rendered as "0 B" beside a populated table.
    assert_ne!(before.bytes, Some(0));
    if accounts_for_size_immediately(&pool) {
        // The size, by contrast, is known from the moment the table has pages
        // — the two halves really are independent.
        assert!(
            before.bytes.is_some_and(|b| b > 0),
            "a populated table occupies disk: {:?}",
            before.bytes
        );
    }

    pool.query("ANALYZE stats_intro").await.unwrap();
    let after = pool.fetch_table_stats(&meta).await.unwrap();
    assert_eq!(
        after.rows,
        Some(RowCount::Estimated(4)),
        "ANALYZE measured four rows, and the number must arrive labelled as an estimate"
    );
    assert!(after.rows.is_some_and(RowCount::is_estimate));

    // The exact count is the same number arriving a completely different way,
    // and it must never be confused for the estimate.
    assert_eq!(pool.count_table_rows(&meta).await.unwrap(), 4);
    assert_ne!(after.rows, Some(RowCount::Exact(4)));

    pool.query("DROP TABLE stats_intro").await.unwrap();
    pool.close().await;
}

#[tokio::test]
async fn a_view_reports_no_size_because_it_occupies_none() {
    let Some(url) = test_url().await else { return };
    let pool = DbPool::open_postgres(&url).await.unwrap();
    // The dependents come down first: `fresh_fixture` drops without CASCADE,
    // so a leftover view from an interrupted run would otherwise pin the base
    // table and fail the setup rather than the assertion.
    pool.query("DROP VIEW IF EXISTS stats_view").await.unwrap();
    pool.query("DROP MATERIALIZED VIEW IF EXISTS stats_matview")
        .await
        .unwrap();
    fresh_fixture(&pool, "stats_base").await;
    pool.query("CREATE VIEW stats_view AS SELECT * FROM stats_base")
        .await
        .unwrap();
    pool.query("CREATE MATERIALIZED VIEW stats_matview AS SELECT * FROM stats_base")
        .await
        .unwrap();

    // `pg_total_relation_size` answers 0 for a plain view, which would render
    // as a measured "0 B" rather than as the absence it is.
    let view = pool
        .fetch_table_stats(&meta_of(&pool, "stats_view").await)
        .await
        .unwrap();
    assert_eq!(view.bytes, None, "a view has no storage to report");
    assert_eq!(view.rows, None);
    assert!(view.is_empty());

    // A materialized view does have storage, and the relkind test must not
    // sweep it up with the plain one.
    let matview = pool
        .fetch_table_stats(&meta_of(&pool, "stats_matview").await)
        .await
        .unwrap();
    if accounts_for_size_immediately(&pool) {
        assert!(
            matview.bytes.is_some_and(|b| b > 0),
            "a materialized view stores its rows: {:?}",
            matview.bytes
        );
    }

    // Counting a view exactly is still meaningful — it is the rows it would
    // return, and it is the only number available for one.
    assert_eq!(
        pool.count_table_rows(&meta_of(&pool, "stats_view").await)
            .await
            .unwrap(),
        4
    );

    pool.query("DROP VIEW stats_view").await.unwrap();
    pool.query("DROP MATERIALIZED VIEW stats_matview")
        .await
        .unwrap();
    pool.query("DROP TABLE stats_base").await.unwrap();
    pool.close().await;
}

#[tokio::test]
async fn an_exact_count_ignores_a_quoted_name_and_a_non_public_schema() {
    let Some(url) = test_url().await else { return };
    let pool = DbPool::open_postgres(&url).await.unwrap();
    pool.query("CREATE SCHEMA IF NOT EXISTS \"stats sch\"")
        .await
        .unwrap();
    pool.query("DROP TABLE IF EXISTS \"stats sch\".\"odd.name\"")
        .await
        .unwrap();
    pool.query("CREATE TABLE \"stats sch\".\"odd.name\" (id int)")
        .await
        .unwrap();
    pool.query("INSERT INTO \"stats sch\".\"odd.name\" VALUES (1), (2)")
        .await
        .unwrap();
    pool.query("ANALYZE \"stats sch\".\"odd.name\"")
        .await
        .unwrap();

    let meta = pool
        .introspect()
        .await
        .unwrap()
        .into_iter()
        .find(|t| t.name == "odd.name" && t.schema.as_deref() == Some("stats sch"))
        .expect("qualified fixture missing from introspection");

    // Both paths qualify and quote through the same helper the rest of the
    // app uses; a schema with a space and a table with a dot in its name are
    // where a hand-built name would silently resolve elsewhere.
    assert_eq!(pool.count_table_rows(&meta).await.unwrap(), 2);
    let stats = pool.fetch_table_stats(&meta).await.unwrap();
    assert_eq!(stats.rows, Some(RowCount::Estimated(2)));

    pool.query("DROP TABLE \"stats sch\".\"odd.name\"")
        .await
        .unwrap();
    pool.query("DROP SCHEMA \"stats sch\"").await.unwrap();
    pool.close().await;
}

#[tokio::test]
async fn an_analyzed_empty_table_reports_zero_rows_rather_than_nothing() {
    let Some(url) = test_url().await else { return };
    let pool = DbPool::open_postgres(&url).await.unwrap();
    pool.query("DROP TABLE IF EXISTS stats_empty")
        .await
        .unwrap();
    pool.query("CREATE TABLE stats_empty (id int, name text)")
        .await
        .unwrap();

    // Before ANALYZE the table has never been measured: `reltuples` is -1 on
    // every engine this suite is pointed at, and nothing is claimed.
    let meta = meta_of(&pool, "stats_empty").await;
    assert_eq!(pool.fetch_table_stats(&meta).await.unwrap().rows, None);

    pool.query("ANALYZE stats_empty").await.unwrap();
    let stats = pool.fetch_table_stats(&meta).await.unwrap();

    // The state this exists for. ANALYZE measured the table and found no rows,
    // so `reltuples` is 0 — a fact, not an absence. Reporting nothing here
    // would render identically to "statistics unknown" and would contradict
    // SQL Server, whose maintained counter reports 0 for the same table
    // (`sqlserver_an_empty_table_reports_zero_rows_not_nothing`).
    assert_eq!(
        stats.rows,
        Some(RowCount::Estimated(0)),
        "a measured zero must be reported as zero"
    );
    assert!(stats.rows.is_some_and(RowCount::is_estimate));
    // And it is emphatically not "nothing known" — the pane must not fall back
    // to its no-statistics line for a table it can describe.
    assert!(!stats.is_empty());

    assert_eq!(pool.count_table_rows(&meta).await.unwrap(), 0);

    pool.query("DROP TABLE stats_empty").await.unwrap();
    pool.close().await;
}

#[tokio::test]
async fn postgres_explains_a_query_as_a_structured_plan() {
    let Some(url) = test_url().await else { return };
    let pool = DbPool::open_postgres(&url).await.unwrap();
    fresh_fixture(&pool, "fruits_plan").await;

    // Stock PostgreSQL is the one flavor that gets the JSON form (FRE-119).
    let support = pool.explain_support().expect("postgres has EXPLAIN");
    assert!(support.structured);

    let sql = explain_statement(
        "SELECT name FROM fruits_plan WHERE weight > 1 ORDER BY name",
        support,
    );
    assert_eq!(
        sql,
        "EXPLAIN (FORMAT JSON) SELECT name FROM fruits_plan WHERE weight > 1 ORDER BY name"
    );
    let result = pool.query(&sql).await.unwrap();

    // The whole point of the structured path: a real server's output parses
    // into a tree, not into the raw-text fallback. A `FORMAT JSON` column that
    // stopped decoding as JSON — or a plan shape this parser stopped
    // recognizing — would land in Raw and still "work", which is exactly the
    // silent degrade this asserts against.
    let PlanDisplay::Tree(tree) = PlanDisplay::from_result(support.structured, &result) else {
        panic!("a real postgres plan must parse as a tree, not degrade to raw text");
    };
    let rows = tree.rows();
    // A sort over a scan: at least two nodes, parent before child.
    assert!(rows.len() >= 2, "{rows:?}");
    assert_eq!(rows[0].0, 0);
    assert_eq!(rows[1].0, 1);
    assert_eq!(rows[0].1.node_type, "Sort");
    assert!(rows.iter().any(|(_, node)| node.node_type.contains("Scan")));
    // Every node carries the numbers the view exists to show, and the scan
    // names the table it reads.
    for (_, node) in &rows {
        assert!(node.total_cost.is_some(), "{node:?}");
        assert!(node.plan_rows.is_some(), "{node:?}");
        // A plain EXPLAIN measures nothing — the statement did not run.
        assert_eq!(node.actual_rows, None, "{node:?}");
    }
    assert!(rows
        .iter()
        .any(|(_, node)| node.label().contains("on fruits_plan")));
    assert_eq!(tree.execution_ms, None);
    // Costs are shares of the same total, and something has to hold the cost.
    assert_eq!(tree.total_cost, tree.root.total_cost.unwrap());
    assert!(rows.iter().any(|(_, node)| node.expensive));

    pool.query("DROP TABLE fruits_plan").await.unwrap();
    pool.close().await;
}

/// The `fruits_guard` names, in id order — what "did the statement run?" is
/// asked of.
async fn guard_names(pool: &DbPool) -> Vec<String> {
    pool.query("SELECT name FROM fruits_guard ORDER BY id")
        .await
        .unwrap()
        .rows
        .iter()
        .map(|row| row[0].display())
        .collect()
}

#[tokio::test]
async fn postgres_explain_analyze_of_a_write_cannot_slip_past_the_gate() {
    let Some(url) = test_url().await else { return };
    let pool = DbPool::open_postgres(&url).await.unwrap();
    fresh_fixture(&pool, "fruits_guard").await;
    let before = guard_names(&pool).await;

    // What the Explain action generates for an UPDATE: a plain EXPLAIN, which
    // costs the statement without running it. Asserted against the table
    // rather than against the plan, because "did it run?" is the only question
    // that matters here.
    let planned = explain_statement(
        "UPDATE fruits_guard SET name = 'planned'",
        pool.explain_support().unwrap(),
    );
    pool.query(&planned).await.unwrap();
    assert_eq!(
        guard_names(&pool).await,
        before,
        "a plain EXPLAIN must not write"
    );

    // Why the gate exists: the same statement with ANALYZE — which hubro never
    // adds, but a user can type — really does write. If this ever stops
    // changing rows, the guard below is guarding nothing and the tests that
    // rely on it are worth nothing.
    let analyzed = "EXPLAIN (ANALYZE, FORMAT JSON) UPDATE fruits_guard SET name = 'analyzed'";
    assert!(needs_confirmation(analyzed, Dialect::Postgres));

    // The gate itself, at the layer the SQL pane calls: a connection whose
    // capabilities forbid writes refuses the statement outright, and the run
    // reports that nothing was sent.
    let refused = run_script(
        &pool,
        Capabilities::FULL.read_only(),
        &[analyzed.to_string()],
        |_| panic!("a refused script must not execute a statement"),
    )
    .await
    .expect_err("a read-only connection must refuse EXPLAIN ANALYZE of a write");
    assert!(matches!(refused.error, DbError::Unsupported(_)));
    assert_eq!(refused.rollback, Rollback::None);
    assert_eq!(
        guard_names(&pool).await,
        before,
        "a refused EXPLAIN ANALYZE must leave the table untouched"
    );

    // And with the capability, it writes — the fact the refusal above is
    // protecting against, proved on the same server in the same test.
    run_script(&pool, Capabilities::FULL, &[analyzed.to_string()], |_| {})
        .await
        .unwrap();
    assert!(guard_names(&pool)
        .await
        .iter()
        .all(|name| name == "analyzed"));

    pool.query("DROP TABLE fruits_guard").await.unwrap();
    pool.close().await;
}

/// Every `EXPLAIN` header spelling hubro's header skipper tolerates is
/// spelling a real PostgreSQL server accepts — and none of them costs a read
/// its capability.
///
/// The list here is the PostgreSQL-valid half of `EXPLAIN_HEADERS` in
/// `src/db/script.rs`'s tests (that one also carries SQLite's `EXPLAIN QUERY
/// PLAN`, which this server would reject). The two exist as a pair: the unit
/// test makes a missing spelling fail a test instead of a user's read-only
/// connection, and this one keeps the list from drifting into syntax only
/// hubro believes in.
///
/// `ANALYSE` is why. PostgreSQL takes the British spelling as a synonym for
/// `ANALYZE`; hubro's skipper did not, so the option became the "statement",
/// classified as a write, and `EXPLAIN ANALYSE SELECT 1` was refused on a
/// read-only connection. Nothing offline could have told us the word is real.
#[tokio::test]
async fn postgres_accepts_every_explain_header_hubro_tolerates() {
    let Some(url) = test_url().await else { return };
    let pool = DbPool::open_postgres(&url).await.unwrap();

    for header in [
        "EXPLAIN",
        "explain",
        "EXPLAIN ANALYZE",
        "EXPLAIN ANALYSE",
        "EXPLAIN VERBOSE",
        "EXPLAIN ANALYZE VERBOSE",
        "explain analyse verbose",
        "EXPLAIN (FORMAT JSON)",
        "EXPLAIN (ANALYZE, FORMAT JSON)",
        "EXPLAIN (ANALYZE, BUFFERS, VERBOSE, FORMAT JSON)",
        "EXPLAIN (COSTS OFF)",
        "EXPLAIN /* just looking */ ANALYZE",
    ] {
        let sql = format!("{header} SELECT 1");
        pool.query(&sql)
            .await
            .unwrap_or_else(|e| panic!("postgres rejected {sql:?}: {e}"));
        // And hubro reads it as what it is: a plan of a read.
        assert!(
            !needs_confirmation(&sql, Dialect::Postgres),
            "{sql:?} prompts to confirm a read"
        );
        assert_eq!(
            script_refusal(
                Capabilities::FULL.read_only(),
                std::slice::from_ref(&sql),
                Dialect::Postgres
            ),
            None,
            "a read-only connection refuses {sql:?}"
        );
    }

    pool.close().await;
}
