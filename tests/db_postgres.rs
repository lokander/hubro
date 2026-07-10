//! Postgres integration tests. They need a running server (Docker only, per
//! CLAUDE.md) and are skipped unless `DATAVIEW_PG_TEST_URL` is set, e.g.:
//!
//! ```sh
//! docker run -d --name dataview-pg-test -e POSTGRES_PASSWORD=testpass \
//!   -e POSTGRES_USER=tester -e POSTGRES_DB=demo -p 5433:5432 postgres:17-alpine
//! DATAVIEW_PG_TEST_URL=postgres://tester:testpass@localhost:5433/demo cargo test
//! ```

use dataview::db::{
    url_with_password, DbError, DbPool, Filter, FilterOp, PageRequest, SortDir, TableKind, Value,
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
        table: "fruits_page".into(),
        limit: 2,
        offset: 0,
        sort: Some(("name".into(), SortDir::Asc)),
        filter: None,
    };
    assert_eq!(pool.count_rows(&request).await.unwrap(), 4);
    let page = pool.fetch_page(&request).await.unwrap();
    assert_eq!(page.rows.len(), 2);
    assert_eq!(page.rows[0][1], Value::Text("a_c".into()));
    assert_eq!(page.rows[1][1], Value::Text("apple".into()));

    // Contains filter with an underscore matches literally, not as a wildcard.
    request.filter = Some(Filter {
        column: "name".into(),
        op: FilterOp::Contains,
        value: "a_".into(),
    });
    request.limit = 10;
    let filtered = pool.fetch_page(&request).await.unwrap();
    assert_eq!(filtered.rows.len(), 1);
    assert_eq!(filtered.rows[0][1], Value::Text("a_c".into()));

    // Equals filter on a numeric column via the ::text cast.
    request.filter = Some(Filter {
        column: "id".into(),
        op: FilterOp::Equals,
        value: "1".into(),
    });
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
