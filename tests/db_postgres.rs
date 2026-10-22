//! Postgres integration tests. They need a running server (Docker only, per
//! CLAUDE.md) and are skipped unless `HUBRO_PG_TEST_URL` is set, e.g.:
//!
//! ```sh
//! docker run -d --name hubro-pg-test -e POSTGRES_PASSWORD=testpass \
//!   -e POSTGRES_USER=tester -e POSTGRES_DB=demo -p 5433:5432 postgres:17-alpine
//! HUBRO_PG_TEST_URL=postgres://tester:testpass@localhost:5433/demo cargo test
//! ```
//!
//! The suite runs in a database of its own, created by `common::pg_test_url`:
//! the Postgres suites all use the same fixture names, so any two
//! running at once against one database dropped each other's tables. Cargo
//! runs test targets sequentially, so the trigger is two concurrent `cargo
//! test` invocations rather than one.

mod common;

use hubro::db::{
    apply_staged, detect_row_identity, explain_statement, needs_confirmation,
    plan_reached_cell_cap, run_script, script_refusal, url_with_password, Capabilities, DbError,
    DbPool, Dialect, Filter, Internal, PageRequest, PgFlavor, PlanDisplay, Restriction, Rollback,
    RowCount, RowLocator, SortDir, StagedChange, StatementOutcome, TableKind, TableMeta,
    TypeDetail, TypeRef, Value, WriteProtection, INSTEAD_RULE, PREVIEW_BYTES, QUERY_CELL_CAP,
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

    // The safety claim behind flavor detection, asserted from the
    // side CI can actually reach: an engine-specific branch must never fire
    // against real PostgreSQL. `tests/db_cockroach.rs` pins the other
    // direction, but that binary needs a Cockroach container and CI has none —
    // so without this, a detection change that swept stock Postgres into an
    // engine branch would pass everything CI runs. CI's Linux leg starts the
    // Postgres this needs and requires its variable (`ci.yml`,
    // `ci_linux_runs_the_postgres_suites` in tests/common); before it did, this
    // skipped there like the rest.
    assert_eq!(pool.pg_flavor(), Some(PgFlavor::Postgres));

    pool.close().await;
}

/// Makes the server send a genuine `40001` on demand, with no race: a
/// REPEATABLE READ transaction takes its snapshot, another session commits an
/// update to the same row, and the transaction's own update of that row is
/// then refused as a serialization failure. The order is enforced by the
/// awaits, not by timing, so it fails the same way on every machine.
///
/// Returns the refused statement's error, as hubro reported it.
async fn force_serialization_failure(pool: &DbPool, table: &str) -> DbError {
    pool.execute(&format!("DROP TABLE IF EXISTS {table}"))
        .await
        .unwrap();
    pool.execute(&format!("CREATE TABLE {table} (id int PRIMARY KEY, v int)"))
        .await
        .unwrap();
    pool.execute(&format!("INSERT INTO {table} VALUES (1, 0)"))
        .await
        .unwrap();

    let mut tx = pool.begin_script_tx().await.unwrap();
    tx.execute("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ")
        .await
        .unwrap();
    // The snapshot is taken by the first query, not by BEGIN.
    tx.query_capped(&format!("SELECT v FROM {table} WHERE id = 1"), 1)
        .await
        .unwrap();
    pool.execute(&format!("UPDATE {table} SET v = 1 WHERE id = 1"))
        .await
        .unwrap();
    let refused = tx
        .execute(&format!("UPDATE {table} SET v = 2 WHERE id = 1"))
        .await
        .expect_err("a REPEATABLE READ update of a row changed since its snapshot must be refused");
    let _ = tx.rollback().await;
    pool.execute(&format!("DROP TABLE {table}")).await.unwrap();
    refused
}

#[tokio::test]
async fn a_server_sent_serialization_failure_classifies_as_transient() {
    let Some(url) = test_url().await else { return };
    let pool = DbPool::open_postgres(&url).await.unwrap();

    // The transient classification (SQLSTATE `40001`) is what the catalog
    // reads' one retry hangs off, and until this test it was pinned against a
    // live server only by db_yugabyte.rs's DDL race — which needs a Yugabyte
    // container CI does not have, and which asserts nothing on a run where
    // neither statement happens to lose.
    let refused = force_serialization_failure(&pool, "serial_conflict").await;
    assert!(
        refused.is_transient(),
        "a server-sent 40001 must classify as transient, got: {refused:?}"
    );
    assert!(
        refused.message().contains("could not serialize access"),
        "{refused:?}"
    );

    // And the classification is the code's, not every failure's.
    let other = pool.query("SELECT 1 / 0").await.unwrap_err();
    assert!(!other.is_transient(), "{other:?}");

    pool.close().await;
}

#[tokio::test]
async fn a_deadlock_is_not_classified_as_transient() {
    let Some(url) = test_url().await else { return };
    let pool = DbPool::open_postgres(&url).await.unwrap();

    // The nearest neighbour of `40001`: a deadlock (`40P01`) is retryable in
    // the same sense, and postgres.rs leaves it out on purpose — it means
    // genuine contention, not a racing schema change. Nothing pinned that
    // choice: classifying `40P01` too passed the whole suite. Two transactions
    // each lock one row and then reach for the other's, so the server must
    // abort one of them; which one is its call, so whichever failed is read.
    pool.execute("DROP TABLE IF EXISTS deadlock_pair")
        .await
        .unwrap();
    pool.execute("CREATE TABLE deadlock_pair (id int PRIMARY KEY)")
        .await
        .unwrap();
    pool.execute("INSERT INTO deadlock_pair VALUES (1), (2)")
        .await
        .unwrap();
    let mut first = pool.begin_script_tx().await.unwrap();
    let mut second = pool.begin_script_tx().await.unwrap();
    let lock = |id: u8| format!("SELECT id FROM deadlock_pair WHERE id = {id} FOR UPDATE");
    first.query_capped(&lock(1), 1).await.unwrap();
    second.query_capped(&lock(2), 1).await.unwrap();
    let (cross_first, cross_second) = (lock(2), lock(1));
    let (a, b) = tokio::join!(
        first.query_capped(&cross_first, 1),
        second.query_capped(&cross_second, 1)
    );
    let _ = first.rollback().await;
    let _ = second.rollback().await;
    let deadlock = match (a, b) {
        (Err(e), Ok(_)) | (Ok(_), Err(e)) => e,
        (a, b) => panic!("expected exactly one deadlock victim, got {a:?} and {b:?}"),
    };
    assert!(
        deadlock.message().contains("deadlock detected"),
        "{deadlock:?}"
    );
    assert!(
        !deadlock.is_transient(),
        "a deadlock is deliberately not classified as transient: {deadlock:?}"
    );
    pool.execute("DROP TABLE deadlock_pair").await.unwrap();

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
    // value never went through f64 (which holds ~15-17). Wider values are
    // covered in postgres_values_read_as_the_server_prints_them.
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

/// The character types by the names sqlx really reports: `char(n)` is
/// `CHAR` and its array `CHAR[]`, while the catalog's one-byte `"char"` keeps
/// its quotes. Matching `BPCHAR` (the name Postgres itself uses) left
/// `char(n)[]` rendering as the `<char[]>` marker, and `"char"` — every
/// `relkind` and `contype` a catalog query returns — as `<"char">`.
#[tokio::test]
async fn postgres_character_types_render_as_their_text() {
    let Some(url) = test_url().await else { return };
    let pool = DbPool::open_postgres(&url).await.unwrap();
    let result = pool
        .query(
            "SELECT 'ab'::char(3) AS fixed, \
                    ARRAY['a', 'bc', NULL]::char(2)[] AS fixed_array, \
                    'r'::\"char\" AS byte, \
                    '\\303'::\"char\" AS high_byte, \
                    ARRAY['r', 'v']::\"char\"[] AS byte_array, \
                    (SELECT relkind FROM pg_class WHERE relname = 'pg_class') AS relkind",
        )
        .await
        .unwrap();
    assert_eq!(
        result.rows[0],
        [
            // Padded to its declared width, as the server stores it.
            Value::Text("ab ".into()),
            Value::Text("{a ,bc,NULL}".into()),
            Value::Text("r".into()),
            // What psql prints for the same byte.
            Value::Text("\\303".into()),
            Value::Text("{r,v}".into()),
            Value::Text("r".into()),
        ]
    );

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

    // Neither NaN nor 39 significant digits fits rust_decimal, which these
    // once went through; they read in full now, and must still not cost
    // the page.
    assert_eq!(*col("nan_num"), Value::Text("NaN".into()));
    assert_eq!(
        *col("big_num"),
        Value::Text("123456789012345678901234567890123456789".into())
    );
    // sqlx only decodes one-dimensional arrays; a 2-D array degrades.
    assert_eq!(*col("matrix"), Value::Text("<int4[]>".into()));
    // Infinite timestamps/dates panicked inside sqlx's chrono decode; they
    // render like psql does.
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
    // CredentialsRejected is what raises the password prompt and, for a
    // remembered password, deletes it — built from SQLSTATE 28P01.
    match err {
        DbError::CredentialsRejected(msg) => assert!(
            msg.contains("authentication failed"),
            "unexpected message: {msg}"
        ),
        other => panic!("expected CredentialsRejected, got {other:?}"),
    }
}

/// How opening `url` failed — `None` if it connected.
async fn connect_failure(url: &str) -> Option<DbError> {
    DbPool::open_postgres(url).await.err()
}

#[tokio::test]
async fn postgres_only_a_refused_password_is_a_credential_rejection() {
    // A rejection prompts for a new password, whose answer can replace or
    // delete the stored one, so each failure here that still has a correct
    // password must stay a plain Connect.
    let Some(url) = test_url().await else { return };
    let admin = DbPool::open_postgres(&url).await.unwrap();
    let database: String = url::Url::parse(&url).unwrap().path()[1..].to_string();
    // Roles are cluster-wide, so named per process.
    let role = format!("hubro_outsider_{}", std::process::id());
    const PASSWORD: &str = "outsider-pass";
    // Every URL is built before the role and the revoke exist, so nothing
    // between them and their undo can panic but a connect that succeeded.
    let missing = common::with_database(&url, "hubro_no_such_database");
    let mut forbidden = url::Url::parse(&url).unwrap();
    forbidden.set_username(&role).unwrap();
    forbidden.set_password(Some(PASSWORD)).unwrap();
    let wrong_and_missing_url = url_with_password(&missing, "definitely-wrong").unwrap();
    admin
        .query(&format!("DROP ROLE IF EXISTS {role}"))
        .await
        .unwrap();
    admin
        .query(&format!("CREATE ROLE {role} LOGIN PASSWORD '{PASSWORD}'"))
        .await
        .unwrap();
    // This binary's own database, which nothing else connects to as anyone
    // but the superuser the suite runs as — and a superuser ignores CONNECT.
    admin
        .query(&format!(
            "REVOKE CONNECT ON DATABASE \"{database}\" FROM PUBLIC"
        ))
        .await
        .unwrap();

    let mut kept = Vec::new();
    for (what, target) in [
        ("a database that does not exist", missing.clone()),
        (
            "a database this role may not connect to",
            forbidden.to_string(),
        ),
    ] {
        kept.push((what, connect_failure(&target).await));
    }
    // Authentication is checked before the database: a wrong password naming
    // a missing database is still the password's fault, and must still prompt.
    let wrong_and_missing = connect_failure(&wrong_and_missing_url).await;

    // Undone before any assertion can panic: a role outlives the test binary,
    // and nothing else sweeps them.
    admin
        .query(&format!(
            "GRANT CONNECT ON DATABASE \"{database}\" TO PUBLIC"
        ))
        .await
        .unwrap();
    admin.query(&format!("DROP ROLE {role}")).await.unwrap();
    admin.close().await;

    for (what, err) in kept {
        let err = err.unwrap_or_else(|| panic!("{what} connected"));
        assert!(
            matches!(err, DbError::Connect(_)) && !err.credentials_rejected(),
            "{what} must keep the password, got {err:?}"
        );
    }
    let err = wrong_and_missing.expect("a wrong password connected");
    assert!(err.credentials_rejected(), "got {err:?}");
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
        // a unique index on it must NOT make it look editable.
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

    // Materialized view: introspected with its own kind and columns
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

    // Capabilities. Postgres declares the full set at the
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

/// Values wider than the Rust types sqlx would decode them into read as the
/// server prints them. Each of these was wrong before, in one of three ways:
/// a panic that closed the app (a `numeric` like `1e30`, inside
/// rust_decimal), a marker (`NaN`, more than 28 digits, a date past chrono's
/// year 262143), or a different value (`0001-01-01 BC` as `0000-01-01`,
/// `24:00:00` as `00:00:00`, `10000-01-01` as `+10000-01-01`).
///
/// Compared against the server's own `::text` of the same value, so the
/// expectation is the engine's spelling rather than one written here — except
/// for the zoned types, whose text follows the session's time zone and which
/// hubro prints in UTC with a `+00:00` offset.
#[tokio::test]
async fn postgres_values_read_as_the_server_prints_them() {
    let Some(url) = test_url().await else { return };
    let pool = DbPool::open_postgres(&url).await.unwrap();
    let as_printed = [
        "1e30::numeric",
        "(-1e30)::numeric",
        "123456789012345678901234567890123456789::numeric",
        "0.0000000000000000000000000000001::numeric",
        "'NaN'::numeric",
        "'Infinity'::numeric",
        "'-Infinity'::numeric",
        "1.5::numeric(10, 2)",
        "0::numeric(5, 3)",
        "ARRAY[1e30, 'NaN', NULL, 1.50]::numeric[]",
        "'2024-02-29'::date",
        "'0001-01-01 BC'::date",
        "'4714-11-24 BC'::date",
        "'10000-01-01'::date",
        "'5874897-12-31'::date",
        "'infinity'::date",
        "'-infinity'::date",
        "'1999-12-31 23:59:59.999999'::timestamp",
        "'0044-03-15 12:00:00.5 BC'::timestamp",
        "'4714-11-24 00:00:00 BC'::timestamp",
        "'294276-12-31 23:59:59.999999'::timestamp",
        "'infinity'::timestamp",
        "'24:00:00'::time",
        "'23:59:59.999999'::time",
    ];
    let select: Vec<String> = as_printed
        .iter()
        .map(|expr| format!("{expr}, ({expr})::text"))
        .collect();
    let result = pool
        .query(&format!("SELECT {}", select.join(", ")))
        .await
        .unwrap();
    let row = &result.rows[0];
    for (n, expr) in as_printed.iter().enumerate() {
        assert_eq!(row[2 * n], row[2 * n + 1], "{expr}");
    }

    let in_utc = [
        (
            "'0001-01-01 00:00:00+00 BC'::timestamptz",
            "0001-01-01 00:00:00+00:00 BC",
        ),
        (
            "'294276-12-31 23:59:59.999999+00'::timestamptz",
            "294276-12-31 23:59:59.999999+00:00",
        ),
        ("'-infinity'::timestamptz", "-infinity"),
        ("'24:00:00+02'::timetz", "24:00:00+02:00"),
    ];
    let select: Vec<&str> = in_utc.iter().map(|(expr, _)| *expr).collect();
    let result = pool
        .query(&format!("SELECT {}", select.join(", ")))
        .await
        .unwrap();
    for (n, (expr, shown)) in in_utc.iter().enumerate() {
        assert_eq!(result.rows[0][n], Value::Text((*shown).into()), "{expr}");
    }
    pool.close().await;
}

// ---- Bounded-memory reads ---------------------------------------

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

/// A zero-row result still carries its column headers — the same
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

/// A temp table lives on the one pooled connection that made it, so the
/// schema tree cannot list it honestly: the queries that describe it, and
/// every later browse, land on whichever connection is idle. The case most
/// likely to list it is a pool whose every query sees the table, so the test
/// opens exactly that: a single-session pool, where the CREATE, the probe and
/// every query introspection runs share one connection. Without the filter,
/// introspection here lists the table every time.
///
/// It used the app's four-connection pool and relied on a fresh one handing
/// back the same connection, which it does not: sqlx returns a finished
/// connection in a spawned task, so a strictly sequential caller still opens a
/// second one, and which of the two each query got depended on timing. On CI
/// every probe landed on the other connection and the test failed.
///
/// The probe is what keeps this from passing vacuously. Introspection leaving
/// the table out only means something if this pool could see it at all, so
/// each round first asks the same pool whether the catalog lists it, under
/// the very `LOCAL TEMPORARY` label the filter relies on.
#[tokio::test]
async fn postgres_temporary_tables_are_not_listed() {
    let Some(url) = test_url().await else { return };
    let pool = DbPool::open_postgres_single_session(&url).await.unwrap();
    pool.execute("CREATE TEMP TABLE temp_unlisted (email text UNIQUE)")
        .await
        .unwrap();
    for attempt in 0..2 {
        let probe = pool
            .query(
                "SELECT 1 FROM information_schema.tables \
                 WHERE table_name = 'temp_unlisted' AND table_type = 'LOCAL TEMPORARY'",
            )
            .await
            .unwrap();
        assert_eq!(
            probe.rows.len(),
            1,
            "round {attempt}: the pool did not see its own temp table, so nothing here is tested"
        );
        let tables = pool.introspect().await.unwrap();
        let listed: Vec<_> = tables
            .iter()
            .filter(|t| t.name == "temp_unlisted")
            .collect();
        assert!(listed.is_empty(), "introspection {attempt}: {listed:?}");
    }
    pool.close().await;
}

/// Enum and array columns are reported by information_schema as the opaque
/// `USER-DEFINED` / `ARRAY`, so introspection resolves the real structure
/// from pg_catalog for the type-aware editors.
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
/// doesn't exist. Regression test for a bug found reviewing the enum editor.
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

/// A `bit(n)` / `bit varying` column can actually be written.
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
    // an extension's objects. Nothing here involves an extension.
    assert_eq!(find("parted_intro").internal, None);
    assert_eq!(
        find("parted_intro_2026_01").internal,
        Some(Internal::Partition)
    );

    pool.query("DROP TABLE parted_intro CASCADE").await.unwrap();
    pool.close().await;
}

/// Whether this server accounts for an object's size as soon as it is written.
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
    // whole point of rendering absent stats as absent: a viewer that reported
    // this as "0 rows" would be stating something false about a table with
    // four.
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

    // Stock PostgreSQL is the one flavor that gets the JSON form.
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

/// A plan larger than the display cap ordinary query cells get (400
/// partitions: about 90 KB of compact JSON) still reaches the plan view whole,
/// on every path a script can take: alone (autocommit), beside another
/// statement (one transaction), and on a connection marked read-only (the
/// engine-enforced run). A plan cut at 64 KiB does not parse, and the view
/// fell back to raw JSON that stopped mid-string.
#[tokio::test]
async fn postgres_a_plan_larger_than_the_display_cap_still_parses() {
    let Some(url) = test_url().await else { return };
    let pool = DbPool::open_postgres(&url).await.unwrap();
    pool.execute("DROP TABLE IF EXISTS big_plan CASCADE")
        .await
        .unwrap();
    pool.execute("CREATE TABLE big_plan (id int, v text) PARTITION BY RANGE (id)")
        .await
        .unwrap();
    pool.execute(
        "DO $$ BEGIN FOR i IN 0..399 LOOP EXECUTE format(\
         'CREATE TABLE big_plan_%s PARTITION OF big_plan FOR VALUES FROM (%s) TO (%s)', \
         i, i * 10, i * 10 + 10); END LOOP; END $$",
    )
    .await
    .unwrap();
    let support = pool.explain_support().unwrap();
    let plan = explain_statement("SELECT * FROM big_plan", support);

    // The fixture is only worth something if its plan is past the cap.
    let whole = pool.query(&plan).await.unwrap();
    let Value::Text(json) = &whole.rows[0][0] else {
        panic!("{whole:?}");
    };
    assert!(json.len() > QUERY_CELL_CAP, "plan is {} bytes", json.len());

    let caps = pool.backend_capabilities();
    let paths = [
        ("alone", caps, vec![plan.clone()]),
        ("in a transaction", caps, vec![plan.clone(), plan.clone()]),
        // A script with its own transaction control runs on one held
        // connection, the plan in the middle of it.
        (
            "beside its own BEGIN",
            caps,
            vec!["BEGIN".to_string(), plan.clone(), "ROLLBACK".to_string()],
        ),
        (
            "marked read-only",
            WriteProtection::ReadOnly.apply(caps),
            vec![plan.clone()],
        ),
    ];
    for (path, caps, script) in paths {
        let mut results = Vec::new();
        run_script(&pool, caps, &script, |r| results.push(r))
            .await
            .unwrap_or_else(|e| panic!("{path}: {e:?}"));
        assert_eq!(results.len(), script.len(), "{path}");
        for (statement, result) in script.iter().zip(&results) {
            if *statement != plan {
                continue;
            }
            let StatementOutcome::Rows(rows) = &result.outcome else {
                panic!("{path}: {result:?}");
            };
            assert_eq!(rows.rows[0][0], whole.rows[0][0], "{path}: plan cut off");
            assert!(
                matches!(
                    PlanDisplay::from_result(support.structured, rows),
                    PlanDisplay::Tree(_)
                ),
                "{path}: a whole plan must parse as a tree"
            );
            assert!(!plan_reached_cell_cap(rows), "{path}");
        }
    }

    // Ordinary queries keep the display cap on the same path — including
    // one behind a comment that only looks like an EXPLAIN. Postgres nests
    // block comments, so the EXPLAIN below is commented out and the SELECT
    // runs; read with SQLite's rules it took the plan cap and came back at
    // 200,000 bytes.
    for sql in [
        "SELECT repeat('Z', 200000) AS v",
        "/* /* */ EXPLAIN */ SELECT repeat('Z', 200000) AS v",
    ] {
        let mut results = Vec::new();
        run_script(&pool, caps, &[sql.to_string()], |r| results.push(r))
            .await
            .unwrap();
        let StatementOutcome::Rows(rows) = &results[0].outcome else {
            panic!("{sql}: {results:?}");
        };
        let Value::Text(cell) = &rows.rows[0][0] else {
            panic!("{sql}: {rows:?}");
        };
        assert_eq!(cell.len(), QUERY_CELL_CAP, "{sql}");
    }

    pool.execute("DROP TABLE big_plan CASCADE").await.unwrap();
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

/// Key types whose locator has to find its row again — the matrix
/// [`common::row_addressing_failures`] walks. Each entry is a slug, the
/// column type and the key literals stored under it.
///
/// The types from `inet` on have no decoder arm (or, for `oid`, had none):
/// each read as a `<typename>` marker no predicate can re-address, until a
/// key column was fetched through the server's own text
/// (`page::fetched_as_server_text`).
const PG_KEY_TYPES: &[(&str, &str, &[&str])] = &[
    // A float's text spelling is the server's, not Rust's: `float8::text` of
    // 1e20 is `1e+20`, which a key bound as Rust's `Display` never equals.
    (
        "float8",
        "double precision",
        &["1e20", "0.1", "123456789.123456789", "-2.5e-300"],
    ),
    // A `real` key binds as its f32's shortest spelling; the non-finite
    // values and a subnormal are the spellings an engine might not share.
    (
        "float4",
        "real",
        &[
            "1e20",
            "0.1",
            "3.4028235e38",
            "1e-45",
            "'NaN'",
            "'-Infinity'",
        ],
    ),
    // Past rust_decimal's range and its special values: `1e30` used to
    // panic the page fetch, the others to read as the marker `<numeric>`.
    (
        "numeric",
        "numeric",
        &[
            "12345.678901",
            "0.10",
            "1e20",
            "1e30",
            "123456789012345678901234567890.123456789",
            "0.0000000000000000000000000000001",
            "'NaN'",
            "'Infinity'",
        ],
    ),
    ("int8", "bigint", &["9223372036854775807", "-1"]),
    ("uuid", "uuid", &["'a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11'"]),
    // BC values read as astronomical years (`0000-01-01`), which the server
    // refuses back; years past 9999 with a `+` sign, refused the same way.
    (
        "ts",
        "timestamp",
        &[
            "'2024-02-29 13:45:01.123456'",
            "'2024-01-01 00:00:00'",
            "'0044-03-15 12:00:00.5 BC'",
            "'294276-12-31 23:59:59.999999'",
            "'infinity'",
        ],
    ),
    (
        "tstz",
        "timestamptz",
        &[
            "'2024-02-29 13:45:01.123456+02'",
            "'0001-01-01 00:00:00+00 BC'",
            "'-infinity'",
        ],
    ),
    (
        "date",
        "date",
        &[
            "'2024-02-29'",
            "'0001-01-01 BC'",
            "'4714-11-24 BC'",
            "'10000-01-01'",
            "'infinity'",
            "'-infinity'",
        ],
    ),
    // `24:00:00` read as `00:00:00` — the key of a different row.
    (
        "time",
        "time",
        &["'13:45:01.5'", "'24:00:00'", "'00:00:00'"],
    ),
    (
        "timetz",
        "time with time zone",
        &["'13:45:01.5+02'", "'24:00:00-03:30'", "'00:00:00-03:30'"],
    ),
    ("interval", "interval", &["'1 day 02:03:04'"]),
    // A blob key used to be bound as the text `<blob N>`.
    ("bytea", "bytea", &["'\\x00ff10'", "''"]),
    ("text", "text", &["'plain'", "'trailing  '", "'quote''s'"]),
    ("char3", "character(3)", &["'ab'", "'abc'"]),
    ("bool", "boolean", &["true", "false"]),
    ("bit4", "bit(4)", &["B'1010'"]),
    // `{schema}` is the fixture schema, where the enum is created.
    ("mood", "{schema}.mood", &["'happy'", "'Sad'"]),
    // `<inet>` and the rest failed loudly ("invalid input syntax"); the
    // `<tsvector>` marker is itself a valid tsvector, so that key failed
    // silently: its long cell loaded as NULL and its jump found no row.
    //
    // Keys read as text sit beside a neighbour that differs by one detail
    // the text has to keep (a prefix length, a closed bound, a last digit),
    // so a spelling that lost it would name the neighbour: a write that hits
    // the wrong row then shows in the tags the matrix reads back.
    (
        "inet",
        "inet",
        &[
            "'192.168.0.1'",
            "'10.0.0.1/24'",
            "'10.0.0.1/25'",
            "'::ffff:1.2.3.4/120'",
        ],
    ),
    (
        "cidr",
        "cidr",
        &[
            "'10.0.0.0/8'",
            "'10.0.0.0/24'",
            "'10.0.0.0/25'",
            "'2001:db8::/32'",
        ],
    ),
    (
        "macaddr",
        "macaddr",
        &["'08:00:2b:01:02:03'", "'08:00:2b:01:02:04'"],
    ),
    ("macaddr8", "macaddr8", &["'08:00:2b:01:02:03:04:05'"]),
    ("money", "money", &["12.34", "12.30", "12.03", "-0.5"]),
    ("oid", "oid", &["4294967295", "0"]),
    (
        "int4range",
        "int4range",
        &["'[1,5)'", "'[1,5]'", "'(1,5)'", "'empty'", "'(,3]'"],
    ),
    (
        "tsrange",
        "tsrange",
        &[
            "'[2024-01-01 00:00,2024-01-02 12:30)'",
            "'[2024-01-01 00:00,2024-01-02 12:30]'",
        ],
    ),
    // The pair a `SQL, DMY` session would print alike: `[01/02/2024,…)` is
    // the first under DMY and the second under MDY.
    (
        "daterange",
        "daterange",
        &["'[2024-02-01,2024-02-03)'", "'[2024-01-02,2024-03-02)'"],
    ),
    // Equal at 15 significant digits (`extra_float_digits = 0`), distinct
    // at the shortest exact spelling every session is given.
    (
        "float8arr",
        "float8[]",
        &[
            "ARRAY[0.1234567890123456]::float8[]",
            "ARRAY[0.12345678901234567]::float8[]",
        ],
    ),
    (
        "float4arr",
        "float4[]",
        &["ARRAY[0.1]::float4[]", "ARRAY[0.10000001]::float4[]"],
    ),
    // One day minus two hours against one day plus two: what `sql_standard`
    // would and `postgres` would not read alike.
    (
        "intervalarr",
        "interval[]",
        &[
            "ARRAY['-1 days +02:00:00']::interval[]",
            "ARRAY['-1 days -02:00:00']::interval[]",
        ],
    ),
    ("tsvector", "tsvector", &["'a fat cat'", "'quote''s:1A'"]),
    // The decoder's array rendering quotes nothing, so `{a,b,c}` was both
    // of the first two keys and neither; the server's text quotes them.
    (
        "textarr",
        "text[]",
        &[
            "ARRAY['a,b', 'c']",
            "ARRAY['a', 'b,c']",
            "ARRAY['x y', '\"q\"']",
        ],
    ),
];

/// Creates one table per [`PG_KEY_TYPES`] entry in `schema` — `k_<slug>`,
/// keyed by that type, with a long `body` and a short `tag` — plus a
/// composite-keyed `k_composite`, and beside each a `fk_<slug>` whose rows
/// reference every key through a foreign key of the same type, with
/// `expect` naming the body each one must land on.
async fn create_key_type_fixtures(pool: &DbPool, schema: &str) {
    for sql in [
        format!("DROP SCHEMA IF EXISTS {schema} CASCADE"),
        format!("CREATE SCHEMA {schema}"),
        format!("CREATE TYPE {schema}.mood AS ENUM ('happy', 'Sad')"),
    ] {
        pool.query(&sql).await.unwrap();
    }
    for (slug, type_name, literals) in PG_KEY_TYPES {
        let type_name = type_name.replace("{schema}", schema);
        pool.query(&format!(
            "CREATE TABLE {schema}.k_{slug} (k {type_name} PRIMARY KEY, body text, tag text)"
        ))
        .await
        .unwrap();
        pool.query(&format!(
            "CREATE TABLE {schema}.fk_{slug} (
                ref {type_name} REFERENCES {schema}.k_{slug} (k), expect text)"
        ))
        .await
        .unwrap();
        let rows: Vec<String> = literals
            .iter()
            .enumerate()
            .map(|(n, lit)| format!("({lit}, '{n}:' || repeat('x', 5000), 't')"))
            .collect();
        pool.query(&format!(
            "INSERT INTO {schema}.k_{slug} VALUES {}",
            rows.join(", ")
        ))
        .await
        .unwrap_or_else(|e| panic!("{slug}: {e}"));
        let refs: Vec<String> = literals
            .iter()
            .enumerate()
            .map(|(n, lit)| format!("({lit}, '{n}:')"))
            .collect();
        pool.query(&format!(
            "INSERT INTO {schema}.fk_{slug} VALUES {}",
            refs.join(", ")
        ))
        .await
        .unwrap_or_else(|e| panic!("fk_{slug}: {e}"));
    }
    // A composite key mixing a float with text: the clause covers every
    // column, not just the first.
    for sql in [
        format!(
            "CREATE TABLE {schema}.k_composite (
                a double precision, b text, body text, tag text, PRIMARY KEY (a, b))"
        ),
        format!(
            "INSERT INTO {schema}.k_composite VALUES
                (1e20, 'x', '0:' || repeat('x', 5000), 't'),
                (1e20, 'y', '1:' || repeat('x', 5000), 't')"
        ),
        format!(
            "CREATE TABLE {schema}.fk_composite (
                a double precision, b text, expect text,
                FOREIGN KEY (a, b) REFERENCES {schema}.k_composite (a, b))"
        ),
        format!("INSERT INTO {schema}.fk_composite VALUES (1e20, 'x', '0:'), (1e20, 'y', '1:')"),
    ] {
        pool.query(&sql).await.unwrap();
    }
}

/// The `k_<slug>` tables [`create_key_type_fixtures`] made in `schema`, each
/// with its `fk_<slug>` referencing table, checked to be all of them.
fn key_type_fixture_pairs<'a>(
    tables: &'a [TableMeta],
    schema: &str,
) -> Vec<(&'a TableMeta, &'a TableMeta)> {
    let in_schema = |name: &str| {
        tables
            .iter()
            .find(|t| t.schema.as_deref() == Some(schema) && t.name == name)
            .unwrap_or_else(|| panic!("{schema}.{name} was not introspected"))
    };
    PG_KEY_TYPES
        .iter()
        .map(|(slug, _, _)| *slug)
        .chain(["composite"])
        .map(|slug| {
            (
                in_schema(&format!("k_{slug}")),
                in_schema(&format!("fk_{slug}")),
            )
        })
        .collect()
}

#[tokio::test]
async fn postgres_every_key_type_reads_and_writes_the_same_row() {
    // The expand popup, the truncated-cell editor and the clipboard load a
    // long cell's full value through `fetch_cell`; Save re-addresses the row
    // through the staged writer. Both start from the same locator, so both
    // must find the same row for every key type the grid can hold — a float
    // key that saved but expanded to NULL was the bug this pins.
    let Some(url) = test_url().await else { return };
    let pool = DbPool::open_postgres(&url).await.unwrap();
    create_key_type_fixtures(&pool, "addr_keys").await;
    let tables = pool.introspect().await.unwrap();
    let mut failures = Vec::new();
    for (table, _) in key_type_fixture_pairs(&tables, "addr_keys") {
        failures.extend(common::row_addressing_failures(&pool, table).await);
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));

    pool.query("DROP SCHEMA addr_keys CASCADE").await.unwrap();
    pool.close().await;
}

#[tokio::test]
async fn postgres_a_key_read_as_text_still_sorts_by_its_type() {
    // A key read through its text is `"k"::text AS "k"` in the select list,
    // which a bare `ORDER BY "k"` resolves to first — sorting `[10,20)`
    // before `[9,10)`. The sort names the table's column, so it orders as a
    // range. A previewed column must not be named that way: a `json` one has
    // no ordering, and sorting it failed outright when it was.
    let Some(url) = test_url().await else { return };
    let pool = DbPool::open_postgres(&url).await.unwrap();
    for sql in [
        "DROP TABLE IF EXISTS range_sort",
        "CREATE TABLE range_sort (k int4range PRIMARY KEY, j json)",
        "INSERT INTO range_sort VALUES ('[10,20)', '{\"b\": 1}'), ('[9,10)', '{\"c\": 1}'), \
         ('[100,200)', '{\"a\": 1}')",
    ] {
        pool.query(sql).await.unwrap();
    }
    let table = pool
        .introspect()
        .await
        .unwrap()
        .into_iter()
        .find(|t| t.name == "range_sort")
        .unwrap();
    let sorted = |column: &str| PageRequest {
        schema: table.schema.clone(),
        table: table.name.clone(),
        limit: 10,
        offset: 0,
        sort: Some((column.into(), SortDir::Asc)),
        filter: None,
        extra_key_column: None,
    };
    let page = pool
        .fetch_page_bounded(&sorted("k"), &table.columns, &["k"])
        .await
        .unwrap();
    let keys: Vec<&Value> = page.result.rows.iter().map(|row| &row[0]).collect();
    assert_eq!(
        keys,
        [
            &Value::Text("[9,10)".into()),
            &Value::Text("[10,20)".into()),
            &Value::Text("[100,200)".into()),
        ]
    );
    let page = pool
        .fetch_page_bounded(&sorted("j"), &table.columns, &["k"])
        .await
        .expect("a json column sorts by its preview's text");
    assert_eq!(page.result.rows[0][0], Value::Text("[100,200)".into()));
    pool.query("DROP TABLE range_sort").await.unwrap();
    pool.close().await;
}

/// Saves through each row of `table` — keyed by `k`, named by a unique `v`,
/// with a `tag` to write — `rounds` times over, re-reading every tag after
/// each save: a row's tag must be the last value its own save wrote, so a
/// save that the one-row guard let through onto *another* row shows. Rows are
/// found by `v`, never by the key under test. One line per wrong landing.
async fn saves_by_text_key_land_on_their_rows(
    pool: &DbPool,
    table: &str,
    rounds: usize,
) -> Vec<String> {
    let meta = pool
        .introspect()
        .await
        .unwrap()
        .into_iter()
        .find(|t| t.name == table)
        .unwrap();
    let identity = detect_row_identity(&meta, pool.dialect()).unwrap();
    let access = pool.backend_access(&meta);
    let request = PageRequest {
        schema: meta.schema.clone(),
        table: meta.name.clone(),
        limit: 10,
        offset: 0,
        sort: None,
        filter: None,
        extra_key_column: None,
    };
    let tags = || async {
        let read = pool
            .query(&format!("SELECT v, tag FROM {table} ORDER BY v"))
            .await
            .unwrap();
        read.rows
    };
    let mut expected = tags().await;
    let mut failures = Vec::new();
    for round in 0..rounds {
        for at in 0..expected.len() {
            let name = expected[at][0].clone();
            let page = pool
                .fetch_page_bounded(&request, &meta.columns, &["k"])
                .await
                .unwrap();
            let row = page.result.rows.iter().find(|row| row[1] == name).unwrap();
            let tag = Value::Text(format!("edited {round}"));
            let change = StagedChange::Update {
                locator: RowLocator {
                    identity_values: vec![row[0].clone()],
                },
                column: "tag".into(),
                value: tag.clone(),
            };
            if let Err(e) = apply_staged(pool, &access, &meta, &identity, &[change]).await {
                failures.push(format!("{table} {name:?} round {round}: {e}"));
                continue;
            }
            expected[at][1] = tag;
            let now = tags().await;
            if now != expected {
                failures.push(format!(
                    "{table} {name:?} round {round}: the save landed elsewhere: {now:?}"
                ));
                expected = now;
            }
        }
    }
    failures
}

#[tokio::test]
async fn postgres_a_role_default_never_respells_a_key_for_another_session() {
    // A role's or database's default reaches only the sessions opened after
    // it is set, and the pool opens new ones all the time. Measured with
    // psql: after `ALTER ROLE … SET DateStyle = 'SQL, DMY'` a new session
    // printed `[2024-02-01,2024-02-03)` as `[01/02/2024,03/02/2024)`, which an
    // older `ISO, MDY` session reads as the other row; `IntervalStyle =
    // 'sql_standard'` prints -1 day -2 hours as `-1 2:00:00`, which a
    // `postgres` session reads as -1 day +2 hours. hubro's sessions are given
    // both at login, which beats a role default: `DateStyle` by sqlx itself,
    // `IntervalStyle` by `key_text_pins`.
    let Some(url) = test_url().await else { return };
    let setup = DbPool::open_postgres(&url).await.unwrap();
    let who = setup
        .query("SELECT quote_ident(current_user), quote_ident(current_database())")
        .await
        .unwrap();
    let (Value::Text(role), Value::Text(database)) = (&who.rows[0][0], &who.rows[0][1]) else {
        panic!("{who:?}");
    };
    let scope = format!("ALTER ROLE {role} IN DATABASE {database}");
    // Run on its own task so a failure still reaches the reset below: the
    // role default would otherwise outlive the test. A local task, since the
    // body need not be `Send`.
    let outcome = tokio::task::LocalSet::new()
        .run_until(async {
            tokio::task::spawn_local({
                let url = url.clone();
                let scope = scope.clone();
                async move { role_default_body(&url, &scope).await }
            })
            .await
        })
        .await;
    for setting in ["DateStyle", "IntervalStyle"] {
        setup
            .query(&format!("{scope} RESET {setting}"))
            .await
            .unwrap();
    }
    setup.close().await;
    if let Err(panic) = outcome {
        std::panic::resume_unwind(panic.into_panic());
    }
}

async fn role_default_body(url: &str, scope: &str) {
    let pool = DbPool::open_postgres_two_sessions(url).await.unwrap();
    for sql in [
        "DROP TABLE IF EXISTS role_dates",
        "CREATE TABLE role_dates (k daterange PRIMARY KEY, v text, tag text)",
        "INSERT INTO role_dates VALUES \
         ('[2024-02-01,2024-02-03)', 'feb', 't'), ('[2024-01-02,2024-03-02)', 'jan', 't')",
        "DROP TABLE IF EXISTS role_intervals",
        "CREATE TABLE role_intervals (k interval[] PRIMARY KEY, v text, tag text)",
        "INSERT INTO role_intervals VALUES \
         (ARRAY['-1 days -02:00:00']::interval[], 'minus', 't'), \
         (ARRAY['-1 days +02:00:00']::interval[], 'plus', 't')",
    ] {
        pool.query(sql).await.unwrap();
    }
    // Both sessions of the pool exist before the default changes.
    let probe = "SELECT pg_backend_pid()::int8, current_setting('DateStyle'), \
                 current_setting('IntervalStyle'), pg_sleep(0.3)::text";
    let (a, b) = tokio::join!(pool.query(probe), pool.query(probe));
    let before = [a.unwrap().rows[0][0].clone(), b.unwrap().rows[0][0].clone()];
    for sql in [
        format!("{scope} SET DateStyle = 'SQL, DMY'"),
        format!("{scope} SET IntervalStyle = 'sql_standard'"),
    ] {
        pool.query(&sql).await.unwrap();
    }
    // A script's connection is closed after its run, so the next pair of
    // reads finds one old session and opens one new one.
    let caps = pool.backend_capabilities();
    run_script(&pool, caps, &["SELECT 1".to_string()], |_| {})
        .await
        .unwrap();
    let (a, b) = tokio::join!(pool.query(probe), pool.query(probe));
    let after = [a.unwrap().rows[0].clone(), b.unwrap().rows[0].clone()];
    assert!(
        after.iter().any(|session| !before.contains(&session[0])),
        "no session was opened after the role default changed: {before:?} {after:?}"
    );
    for session in &after {
        assert_eq!(
            session[1..3],
            [
                Value::Text("ISO, MDY".into()),
                Value::Text("postgres".into())
            ],
            "a session took the role's default: {after:?}"
        );
    }
    let mut failures = saves_by_text_key_land_on_their_rows(&pool, "role_dates", 4).await;
    failures.extend(saves_by_text_key_land_on_their_rows(&pool, "role_intervals", 4).await);
    assert!(failures.is_empty(), "{}", failures.join("\n"));
    pool.query("DROP TABLE role_dates, role_intervals")
        .await
        .unwrap();
    pool.close().await;
}

#[tokio::test]
async fn postgres_a_scripts_datestyle_never_respells_a_key_for_another_session() {
    // A key read through `::text` is spelled by the session that read it and
    // parsed back by the session that writes. A script's `SET` used to stay
    // on its pooled connection: measured, `[2024-02-01,2024-02-03)` read
    // under `SQL, DMY` as `[01/02/2024,03/02/2024)`, which another session
    // under `ISO, MDY` parses as January 2 to March 2 — exactly one other
    // row, so the one-row guard passed and the save landed there. A script's
    // connection now closes after its run, so no pooled session carries the
    // script's settings, and the read and the write agree.
    let Some(url) = test_url().await else { return };
    let pool = DbPool::open_postgres_two_sessions(&url).await.unwrap();
    for sql in [
        "DROP TABLE IF EXISTS style_keys",
        "CREATE TABLE style_keys (k daterange PRIMARY KEY, v text, tag text)",
        "INSERT INTO style_keys VALUES \
         ('[2024-02-01,2024-02-03)', 'feb', 't'), ('[2024-01-02,2024-03-02)', 'jan', 't')",
    ] {
        pool.query(sql).await.unwrap();
    }
    // Both ways a script runs: a lone statement on a held connection (the
    // sequential path, which closes it explicitly), and several in hubro's
    // transaction (the atomic one, whose committed `SET` outlives the
    // transaction, and whose connection closes as the transaction drops it).
    let caps = pool.backend_capabilities();
    for script in [
        vec!["SET datestyle = 'SQL, DMY'".to_string()],
        vec![
            "SET datestyle = 'SQL, DMY'".to_string(),
            "SELECT 1".to_string(),
        ],
    ] {
        let mut results = Vec::new();
        run_script(&pool, caps, &script, |r| results.push(r))
            .await
            .unwrap();
        assert_eq!(results.len(), script.len(), "the script ran");
    }

    // Every session the pool holds: two reads kept open at once cannot share
    // a connection on a pool of two.
    let style = "SELECT current_setting('DateStyle'), pg_sleep(0.3)::text";
    let (a, b) = tokio::join!(pool.query(style), pool.query(style));
    for session in [a.unwrap(), b.unwrap()] {
        assert_eq!(
            session.rows[0][0],
            Value::Text("ISO, MDY".into()),
            "a pooled session kept the script's DateStyle"
        );
    }

    // And end to end: the `feb` row is read, re-addressed and saved, and
    // only it changes — several rounds, so reads and writes fall on both
    // sessions.
    let failures = saves_by_text_key_land_on_their_rows(&pool, "style_keys", 6).await;
    assert!(failures.is_empty(), "{}", failures.join("\n"));

    pool.query("DROP TABLE style_keys").await.unwrap();
    pool.close().await;
}

#[tokio::test]
async fn postgres_every_key_type_jumps_to_the_row_it_references() {
    // A foreign-key jump pins the referenced row from the source row's own
    // typed values, so it is the same question as re-addressing a row: a
    // float, blob or date/time key compared as hubro's text spelling of it
    // landed on an empty page.
    let Some(url) = test_url().await else { return };
    let pool = DbPool::open_postgres(&url).await.unwrap();
    create_key_type_fixtures(&pool, "fk_keys").await;
    let tables = pool.introspect().await.unwrap();
    let mut failures = Vec::new();
    for (parent, child) in key_type_fixture_pairs(&tables, "fk_keys") {
        failures.extend(common::fk_jump_failures(&pool, child, parent).await);
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));

    pool.query("DROP SCHEMA fk_keys CASCADE").await.unwrap();
    pool.close().await;
}

#[tokio::test]
async fn postgres_jumps_from_differently_typed_sources_share_no_statement() {
    // sqlx caches a prepared statement by its SQL text alone, with the
    // parameter types of the first execution. A jump's SQL is decided by the
    // *target* column, its bind by the *source* column, so two foreign keys
    // of different types into one column render the same statement with
    // different binds: an `int` key into `numeric` and then a `numeric` one
    // failed outright, and a `double precision` key after an `int` one had
    // its bits read as an int8 and silently landed on an empty page. One
    // session, several rounds, so every jump meets the others' statements.
    // (CockroachDB refuses a foreign key whose type differs from the column
    // it references, so there this shape cannot be built.)
    let Some(url) = test_url().await else { return };
    let pool = DbPool::open_postgres_single_session(&url).await.unwrap();
    for sql in [
        "DROP SCHEMA IF EXISTS mixed_fk CASCADE",
        "CREATE SCHEMA mixed_fk",
        "CREATE TABLE mixed_fk.pnum (id numeric PRIMARY KEY, body text)",
        "INSERT INTO mixed_fk.pnum VALUES (1, '0:'), (2, '1:')",
        "CREATE TABLE mixed_fk.pdbl (id double precision PRIMARY KEY, body text)",
        "INSERT INTO mixed_fk.pdbl VALUES (1, '0:'), (2.5, '1:')",
        "CREATE TABLE mixed_fk.a_int (pid int REFERENCES mixed_fk.pnum, expect text)",
        "INSERT INTO mixed_fk.a_int VALUES (1, '0:')",
        "CREATE TABLE mixed_fk.b_num (pid numeric REFERENCES mixed_fk.pnum, expect text)",
        "INSERT INTO mixed_fk.b_num VALUES (2, '1:')",
        "CREATE TABLE mixed_fk.c_int (pid int REFERENCES mixed_fk.pdbl, expect text)",
        "INSERT INTO mixed_fk.c_int VALUES (1, '0:')",
        "CREATE TABLE mixed_fk.d_dbl (pid double precision REFERENCES mixed_fk.pdbl, expect text)",
        "INSERT INTO mixed_fk.d_dbl VALUES (2.5, '1:')",
    ] {
        pool.query(sql)
            .await
            .unwrap_or_else(|e| panic!("{sql}: {e}"));
    }
    let tables = pool.introspect().await.unwrap();
    let named = |name: &str| {
        tables
            .iter()
            .find(|t| t.schema.as_deref() == Some("mixed_fk") && t.name == name)
            .unwrap_or_else(|| panic!("{name} was not introspected"))
    };
    let mut failures = Vec::new();
    for round in 0..3 {
        for (child, parent) in [
            ("a_int", "pnum"),
            ("b_num", "pnum"),
            ("c_int", "pdbl"),
            ("d_dbl", "pdbl"),
        ] {
            for failure in common::fk_jump_failures(&pool, named(child), named(parent)).await {
                failures.push(format!("round {round}: {failure}"));
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));

    pool.query("DROP SCHEMA mixed_fk CASCADE").await.unwrap();
    pool.close().await;
}

#[tokio::test]
async fn postgres_writes_of_differently_typed_values_share_no_statement() {
    // The same cache, reached by Save: the cell editor stages `42` typed
    // into a `double precision` cell as an integer and `1.5` as a float,
    // and both render `SET "f" = $1::double precision WHERE …`. Whichever
    // ran second on a session was bound against the first one's types — a
    // float's bits read as an int8, or an integer refused by a numeric bind.
    let Some(url) = test_url().await else { return };
    let pool = DbPool::open_postgres_single_session(&url).await.unwrap();
    for sql in [
        "DROP TABLE IF EXISTS mixed_writes",
        "CREATE TABLE mixed_writes (id int PRIMARY KEY, f double precision, n numeric)",
        "INSERT INTO mixed_writes VALUES (1, 0, 0), (2, 0, 0)",
    ] {
        pool.query(sql).await.unwrap();
    }
    let tables = pool.introspect().await.unwrap();
    let table = tables
        .iter()
        .find(|t| t.name == "mixed_writes")
        .expect("introspected");
    let identity = detect_row_identity(table, pool.dialect()).unwrap();
    let access = pool.backend_access(table);
    let set = |id: i64, f: Value, n: Value| {
        let locator = RowLocator {
            identity_values: vec![Value::Integer(id)],
        };
        vec![
            StagedChange::Update {
                locator: locator.clone(),
                column: "f".into(),
                value: f,
            },
            StagedChange::Update {
                locator,
                column: "n".into(),
                value: n,
            },
        ]
    };
    for round in 0..2 {
        for (id, f, n) in [
            (1, Value::Integer(42), Value::Integer(7)),
            (2, Value::Real(1.5), Value::Text("12345.678".into())),
        ] {
            apply_staged(&pool, &access, table, &identity, &set(id, f, n))
                .await
                .unwrap_or_else(|e| panic!("round {round}, row {id}: {e}"));
        }
        let rows = pool
            .query("SELECT f, n::text FROM mixed_writes ORDER BY id")
            .await
            .unwrap()
            .rows;
        assert_eq!(
            rows,
            vec![
                vec![Value::Real(42.0), Value::Text("7".into())],
                vec![Value::Real(1.5), Value::Text("12345.678".into())],
            ],
            "round {round}"
        );
    }

    pool.query("DROP TABLE mixed_writes").await.unwrap();
    pool.close().await;
}

#[tokio::test]
async fn postgres_contains_filter_is_a_substring_search_whatever_the_needle_holds() {
    let Some(url) = test_url().await else { return };
    let pool = DbPool::open_postgres(&url).await.unwrap();
    for sql in [
        "DROP TABLE IF EXISTS like_hostile".to_string(),
        "CREATE TABLE like_hostile (id integer PRIMARY KEY, v text)".to_string(),
        format!(
            "INSERT INTO like_hostile VALUES {}",
            common::like_hostile_rows("")
        ),
    ] {
        pool.query(&sql).await.unwrap();
    }
    let mismatches = common::contains_filter_mismatches(&pool, None, "like_hostile").await;
    assert!(mismatches.is_empty(), "{}", mismatches.join("\n"));
    pool.query("DROP TABLE like_hostile").await.unwrap();
    pool.close().await;
}

/// A `real` once read as the `f32` widened to `f64` — `0.1` showed as
/// `0.10000000149011612` in the grid, the export and the clipboard. It reads
/// as its shortest decimal now, and that decimal must still store the same
/// value when written back.
#[tokio::test]
async fn postgres_a_real_reads_as_its_shortest_decimal_and_writes_back_unchanged() {
    let Some(url) = test_url().await else { return };
    let pool = DbPool::open_postgres(&url).await.unwrap();
    let failures = common::real_round_trip_failures(&pool, "real_trip").await;
    assert!(failures.is_empty(), "{}", failures.join("\n"));
    pool.close().await;
}

/// The grid shows a NaN or an infinity, so the editor must take one back:
/// an unchanged NaN cell re-saves, and a typed `Infinity` stores.
#[tokio::test]
async fn postgres_nan_and_infinity_go_through_the_editor_and_back() {
    let Some(url) = test_url().await else { return };
    let pool = DbPool::open_postgres(&url).await.unwrap();
    let failures = common::non_finite_edit_failures(&pool, "non_finite_edits").await;
    assert!(failures.is_empty(), "{}", failures.join("\n"));
    pool.close().await;
}

#[tokio::test]
async fn postgres_a_do_instead_rule_refuses_editing_before_anything_is_written() {
    // A `DO INSTEAD` rule replaces the statement, and the command tag the
    // guard reads is that of whatever the rule ran: this redirect reports
    // `UPDATE 1` while the table is untouched. A `DO ALSO` rule keeps the
    // statement and its tag, and a view keeps the reason it already had,
    // though every view carries an instead rule of its own (`_RETURN`).
    let Some(url) = test_url().await else { return };
    let pool = DbPool::open_postgres(&url).await.unwrap();
    for sql in [
        "DROP VIEW IF EXISTS rule_view",
        "DROP TABLE IF EXISTS rule_redirected, rule_also, rule_log",
        "CREATE TABLE rule_redirected (id int PRIMARY KEY, name text NOT NULL)",
        "CREATE TABLE rule_also (id int PRIMARY KEY, name text NOT NULL)",
        "CREATE TABLE rule_log (id int PRIMARY KEY)",
        "INSERT INTO rule_redirected VALUES (1, 'first')",
        "INSERT INTO rule_log VALUES (1)",
        "CREATE VIEW rule_view AS SELECT id FROM rule_also",
        "CREATE RULE rule_redirect AS ON UPDATE TO rule_redirected \
         DO INSTEAD UPDATE rule_log SET id = id",
        "CREATE RULE rule_also_log AS ON UPDATE TO rule_also \
         DO ALSO INSERT INTO rule_log VALUES (2)",
    ] {
        pool.query(sql)
            .await
            .unwrap_or_else(|e| panic!("{sql}: {e}"));
    }

    let tables = pool.introspect().await.unwrap();
    let find = |name: &str| tables.iter().find(|t| t.name == name).unwrap().clone();
    for (name, expected) in [
        ("rule_redirected", Some(INSTEAD_RULE)),
        ("rule_also", None),
        ("rule_log", None),
        ("rule_view", Some(Restriction::View)),
    ] {
        assert_eq!(
            pool.backend_access(&find(name)).restriction,
            expected,
            "{name}"
        );
    }

    let table = find("rule_redirected");
    let identity = detect_row_identity(&table, Dialect::Postgres).unwrap();
    let err = apply_staged(
        &pool,
        &pool.backend_access(&table),
        &table,
        &identity,
        &[StagedChange::Update {
            locator: RowLocator {
                identity_values: vec![Value::Integer(1)],
            },
            column: "name".into(),
            value: Value::Text("renamed".into()),
        }],
    )
    .await
    .expect_err("an edit the guard cannot check must be refused");
    assert!(
        err.message.contains(INSTEAD_RULE.message()),
        "{}",
        err.message
    );
    let unchanged = pool
        .query("SELECT name FROM rule_redirected WHERE id = 1")
        .await
        .unwrap();
    assert_eq!(unchanged.rows, vec![vec![Value::Text("first".into())]]);

    pool.query("DROP VIEW rule_view").await.unwrap();
    pool.query("DROP TABLE rule_redirected, rule_also, rule_log")
        .await
        .unwrap();
    pool.close().await;
}
