//! Postgres integration tests. They need a running server (Docker only, per
//! CLAUDE.md) and are skipped unless `DATAVIEW_PG_TEST_URL` is set, e.g.:
//!
//! ```sh
//! docker run -d --name dataview-pg-test -e POSTGRES_PASSWORD=testpass \
//!   -e POSTGRES_USER=tester -e POSTGRES_DB=demo -p 5433:5432 postgres:17-alpine
//! DATAVIEW_PG_TEST_URL=postgres://tester:testpass@localhost:5433/demo cargo test
//! ```

use dataview::db::{
    detect_row_identity, url_with_password, DbError, DbPool, Dialect, Filter, PageRequest,
    RowLocator, SortDir, TableKind, TypeDetail, TypeRef, Value, PREVIEW_BYTES, QUERY_CELL_CAP,
};

fn test_url() -> Option<String> {
    match std::env::var("DATAVIEW_PG_TEST_URL") {
        Ok(url) => Some(url),
        Err(_) => {
            eprintln!("skipping postgres test: DATAVIEW_PG_TEST_URL not set");
            None
        }
    }
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
async fn postgres_introspection_lists_public_tables_and_columns() {
    let Some(url) = test_url() else { return };
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
    let Some(url) = test_url() else { return };
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
    let Some(url) = test_url() else { return };
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
    let Some(url) = test_url() else { return };
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
    let Some(url) = test_url() else { return };
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
    let Some(url) = test_url() else { return };
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
    let Some(url) = test_url() else { return };
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
    let Some(url) = test_url() else { return };
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
    let Some(url) = test_url() else { return };
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
    let Some(url) = test_url() else { return };
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

/// Enum and array columns are reported by information_schema as the opaque
/// `USER-DEFINED` / `ARRAY`, so introspection resolves the real structure
/// from pg_catalog for the type-aware editors (FRE-71).
#[tokio::test]
async fn postgres_introspection_resolves_enum_variants_and_array_columns() {
    let Some(url) = test_url() else { return };
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
    let Some(url) = test_url() else { return };
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
    let applied = dataview::db::apply_staged(
        &pool,
        table,
        &identity,
        &[dataview::db::StagedChange::Update {
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
