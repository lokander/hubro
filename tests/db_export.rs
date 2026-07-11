//! Integration tests for streaming export (`DbPool::export`) on both
//! backends. The SQLite cases always run; the Postgres cases need a running
//! server and are skipped unless `DATAVIEW_PG_TEST_URL` is set (see
//! `tests/db_postgres.rs` for the Docker recipe).

mod common;

use common::FixtureDb;
use dataview::db::{DbPool, ExportFormat, Filter, PageRequest, SortDir, Value};

/// Streams an export into an in-memory buffer, returning (bytes, row_count).
async fn export_to_string(
    pool: &DbPool,
    sql: &str,
    params: &[Value],
    format: ExportFormat,
) -> (String, u64) {
    let mut buf = Vec::new();
    let rows = pool
        .export(sql, params, format, &mut buf)
        .await
        .expect("export succeeds");
    (String::from_utf8(buf).expect("export is valid UTF-8"), rows)
}

/// A small three-row table with an integer, text, a NULL, and a blob.
const EXPORT_SCHEMA: &str = r#"
    CREATE TABLE items (
        id INTEGER PRIMARY KEY,
        name TEXT,
        weight REAL,
        data BLOB
    );
    INSERT INTO items (id, name, weight, data) VALUES
        (1, 'apple', 1.5, x'0102'),
        (2, 'banana', NULL, NULL),
        (3, 'has,comma', 2.5, NULL);
"#;

#[tokio::test]
async fn sqlite_export_csv_streams_all_rows_with_quoting_and_null_and_blob() {
    let fixture = FixtureDb::with_sql(EXPORT_SCHEMA).await;
    let pool = fixture.open().await;

    let (csv, rows) = export_to_string(
        &pool,
        "SELECT id, name, weight, data FROM items ORDER BY id",
        &[],
        ExportFormat::Csv,
    )
    .await;
    assert_eq!(rows, 3);
    assert_eq!(
        csv,
        "id,name,weight,data\n\
         1,apple,1.5,\\x0102\n\
         2,banana,,\n\
         3,\"has,comma\",2.5,\n"
    );

    pool.close().await;
}

#[tokio::test]
async fn sqlite_export_json_streams_all_rows_with_types() {
    let fixture = FixtureDb::with_sql(EXPORT_SCHEMA).await;
    let pool = fixture.open().await;

    let (json, rows) = export_to_string(
        &pool,
        "SELECT id, name, weight, data FROM items ORDER BY id",
        &[],
        ExportFormat::Json,
    )
    .await;
    assert_eq!(rows, 3);
    assert_eq!(
        json,
        "[\n  \
         {\"id\":1,\"name\":\"apple\",\"weight\":1.5,\"data\":\"\\\\x0102\"},\n  \
         {\"id\":2,\"name\":\"banana\",\"weight\":null,\"data\":null},\n  \
         {\"id\":3,\"name\":\"has,comma\",\"weight\":2.5,\"data\":null}\n]\n"
    );

    pool.close().await;
}

#[tokio::test]
async fn sqlite_export_respects_filter_and_sort_via_page_request() {
    let fixture = FixtureDb::with_sql(EXPORT_SCHEMA).await;
    let pool = fixture.open().await;

    // The same query the grid would build for the current view: a `contains`
    // filter on name plus a descending sort, and no paging.
    let request = PageRequest {
        schema: None,
        table: "items".into(),
        limit: 0,
        offset: 0,
        sort: Some(("id".into(), SortDir::Desc)),
        filter: Some(Filter::contains("name", "a")),
        extra_key_column: None,
    };
    let (sql, params) = request.export_sql(pool.dialect());
    let (csv, rows) = export_to_string(&pool, &sql, &params, ExportFormat::Csv).await;

    // Only 'apple', 'banana', 'has,comma' contain 'a' (all three), sorted by
    // id descending. `SELECT *` yields the table's column order.
    assert_eq!(rows, 3);
    assert_eq!(
        csv,
        "id,name,weight,data\n\
         3,\"has,comma\",2.5,\n\
         2,banana,,\n\
         1,apple,1.5,\\x0102\n"
    );

    pool.close().await;
}

#[tokio::test]
async fn sqlite_export_empty_result_still_writes_header_and_empty_array() {
    let fixture = FixtureDb::with_sql(EXPORT_SCHEMA).await;
    let pool = fixture.open().await;

    let (csv, csv_rows) = export_to_string(
        &pool,
        "SELECT id, name FROM items WHERE id < 0",
        &[],
        ExportFormat::Csv,
    )
    .await;
    assert_eq!(csv_rows, 0);
    assert_eq!(csv, "id,name\n");

    let (json, json_rows) = export_to_string(
        &pool,
        "SELECT id, name FROM items WHERE id < 0",
        &[],
        ExportFormat::Json,
    )
    .await;
    assert_eq!(json_rows, 0);
    assert_eq!(json, "[]\n");

    pool.close().await;
}

// ---- Postgres -------------------------------------------------------------

fn pg_url() -> Option<String> {
    match std::env::var("DATAVIEW_PG_TEST_URL") {
        Ok(url) => Some(url),
        Err(_) => {
            eprintln!("skipping postgres export test: DATAVIEW_PG_TEST_URL not set");
            None
        }
    }
}

async fn pg_fixture(pool: &DbPool, table: &str) {
    pool.query(&format!("DROP TABLE IF EXISTS {table}"))
        .await
        .unwrap();
    pool.query(&format!(
        "CREATE TABLE {table} (
            id serial PRIMARY KEY,
            name text,
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
            ('has,comma', 2.5, NULL)"
    ))
    .await
    .unwrap();
}

#[tokio::test]
async fn postgres_export_csv_and_json_match_expected_bytes() {
    let Some(url) = pg_url() else { return };
    let pool = DbPool::open_postgres(&url).await.unwrap();
    pg_fixture(&pool, "export_items").await;

    let (csv, csv_rows) = export_to_string(
        &pool,
        "SELECT id, name, weight, data FROM export_items ORDER BY id",
        &[],
        ExportFormat::Csv,
    )
    .await;
    assert_eq!(csv_rows, 3);
    assert_eq!(
        csv,
        "id,name,weight,data\n\
         1,apple,1.5,\\x0102\n\
         2,banana,,\n\
         3,\"has,comma\",2.5,\n"
    );

    let (json, json_rows) = export_to_string(
        &pool,
        "SELECT id, name, weight, data FROM export_items ORDER BY id",
        &[],
        ExportFormat::Json,
    )
    .await;
    assert_eq!(json_rows, 3);
    assert_eq!(
        json,
        "[\n  \
         {\"id\":1,\"name\":\"apple\",\"weight\":1.5,\"data\":\"\\\\x0102\"},\n  \
         {\"id\":2,\"name\":\"banana\",\"weight\":null,\"data\":null},\n  \
         {\"id\":3,\"name\":\"has,comma\",\"weight\":2.5,\"data\":null}\n]\n"
    );

    pool.close().await;
}

#[tokio::test]
async fn postgres_export_respects_filter_and_sort() {
    let Some(url) = pg_url() else { return };
    let pool = DbPool::open_postgres(&url).await.unwrap();
    pg_fixture(&pool, "export_filtered").await;

    let request = PageRequest {
        schema: Some("public".into()),
        table: "export_filtered".into(),
        limit: 0,
        offset: 0,
        sort: Some(("id".into(), SortDir::Desc)),
        filter: Some(Filter::equals("name", "banana")),
        extra_key_column: None,
    };
    let (sql, params) = request.export_sql(pool.dialect());
    let (csv, rows) = export_to_string(&pool, &sql, &params, ExportFormat::Csv).await;

    assert_eq!(rows, 1);
    assert_eq!(csv, "id,name,weight,data\n2,banana,,\n");

    pool.close().await;
}
