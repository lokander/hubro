//! Integration tests for paged table reads: `DbPool::fetch_page` and
//! `DbPool::count_rows` executing real `PageRequest`s against SQLite.

mod common;

use common::FixtureDb;
use dataview::db::{Filter, FilterOp, PageRequest, SortDir, Value};

fn request(table: &str) -> PageRequest {
    PageRequest {
        schema: None,
        table: table.into(),
        limit: 10,
        offset: 0,
        sort: None,
        filter: None,
        extra_key_column: None,
    }
}

fn sorted(table: &str, column: &str, dir: SortDir) -> PageRequest {
    PageRequest {
        schema: None,
        sort: Some((column.into(), dir)),
        ..request(table)
    }
}

/// The `n` column of every returned row (first column of `numbers`).
fn n_values(rows: &[Vec<Value>]) -> Vec<i64> {
    rows.iter()
        .map(|row| match &row[0] {
            Value::Integer(n) => *n,
            other => panic!("expected integer n, got {other:?}"),
        })
        .collect()
}

#[tokio::test]
async fn first_page_is_full_and_last_page_is_partial() {
    let fixture = FixtureDb::numbers(25).await;
    let pool = fixture.open().await;

    let mut req = sorted("numbers", "n", SortDir::Asc);
    let first = pool.fetch_page(&req).await.unwrap();
    assert_eq!(n_values(&first.rows), (1..=10).collect::<Vec<_>>());

    req.offset = 20;
    let last = pool.fetch_page(&req).await.unwrap();
    assert_eq!(n_values(&last.rows), [21, 22, 23, 24, 25]);

    pool.close().await;
}

#[tokio::test]
async fn offset_beyond_the_end_yields_an_empty_page() {
    let fixture = FixtureDb::numbers(25).await;
    let pool = fixture.open().await;

    let req = PageRequest {
        schema: None,
        offset: 100,
        ..request("numbers")
    };
    let page = pool.fetch_page(&req).await.unwrap();
    assert!(page.rows.is_empty());

    pool.close().await;
}

#[tokio::test]
async fn count_rows_ignores_limit_and_offset() {
    let fixture = FixtureDb::numbers(25).await;
    let pool = fixture.open().await;

    let req = PageRequest {
        schema: None,
        limit: 3,
        offset: 100,
        ..request("numbers")
    };
    assert_eq!(pool.count_rows(&req).await.unwrap(), 25);

    pool.close().await;
}

#[tokio::test]
async fn sort_desc_reverses_the_order() {
    let fixture = FixtureDb::numbers(25).await;
    let pool = fixture.open().await;

    let req = sorted("numbers", "n", SortDir::Desc);
    let page = pool.fetch_page(&req).await.unwrap();
    assert_eq!(n_values(&page.rows), (16..=25).rev().collect::<Vec<_>>());

    pool.close().await;
}

#[tokio::test]
async fn sort_places_nulls_first_ascending_and_last_descending() {
    // score is NULL on every fifth row: n = 5, 10, 15, 20, 25.
    let fixture = FixtureDb::numbers(25).await;
    let pool = fixture.open().await;

    let asc = pool
        .fetch_page(&sorted("numbers", "score", SortDir::Asc))
        .await
        .unwrap();
    // SQLite treats NULL as smaller than every value: NULLs lead ascending.
    assert!(asc.rows[..5].iter().all(|row| row[2] == Value::Null));
    assert_eq!(asc.rows[5][2], Value::Real(0.5));

    let desc_req = PageRequest {
        schema: None,
        limit: 25,
        ..sorted("numbers", "score", SortDir::Desc)
    };
    let desc = pool.fetch_page(&desc_req).await.unwrap();
    assert_eq!(desc.rows[0][2], Value::Real(12.0));
    assert!(desc.rows[20..].iter().all(|row| row[2] == Value::Null));

    pool.close().await;
}

#[tokio::test]
async fn contains_filter_treats_percent_and_underscore_literally() {
    let fixture = FixtureDb::with_sql(
        r#"
        CREATE TABLE notes (id INTEGER PRIMARY KEY, body TEXT);
        INSERT INTO notes (id, body) VALUES
            (1, '100% done'),
            (2, '50 percent'),
            (3, 'a_b'),
            (4, 'axb'),
            (5, 'plain');
        "#,
    )
    .await;
    let pool = fixture.open().await;

    let mut req = request("notes");

    // '%' in the needle must not act as a wildcard...
    req.filter = Some(Filter {
        column: "body".into(),
        op: FilterOp::Contains,
        value: "%".into(),
    });
    let percent = pool.fetch_page(&req).await.unwrap();
    assert_eq!(percent.rows.len(), 1);
    assert_eq!(percent.rows[0][1], Value::Text("100% done".into()));
    assert_eq!(pool.count_rows(&req).await.unwrap(), 1);

    // ...and '_' must not match any single character ('axb' stays out).
    req.filter = Some(Filter {
        column: "body".into(),
        op: FilterOp::Contains,
        value: "a_b".into(),
    });
    let underscore = pool.fetch_page(&req).await.unwrap();
    assert_eq!(underscore.rows.len(), 1);
    assert_eq!(underscore.rows[0][1], Value::Text("a_b".into()));

    pool.close().await;
}

#[tokio::test]
async fn equals_filter_with_text_value_matches_numeric_column_via_affinity() {
    let fixture = FixtureDb::numbers(25).await;
    let pool = fixture.open().await;

    // Filter values are always strings from the UI; the INTEGER affinity of
    // `n` must coerce '7' so the comparison matches numerically.
    let req = PageRequest {
        schema: None,
        filter: Some(Filter {
            column: "n".into(),
            op: FilterOp::Equals,
            value: "7".into(),
        }),
        ..request("numbers")
    };
    let page = pool.fetch_page(&req).await.unwrap();
    assert_eq!(n_values(&page.rows), [7]);
    assert_eq!(pool.count_rows(&req).await.unwrap(), 1);

    pool.close().await;
}

#[tokio::test]
async fn filter_sort_and_paging_combine() {
    let fixture = FixtureDb::numbers(25).await;
    let pool = fixture.open().await;

    // 'row 1' matches labels 'row 10'..'row 19' (labels are zero-padded, so
    // n = 1 is 'row 01' and stays out): 10 rows total.
    let mut req = PageRequest {
        schema: None,
        limit: 3,
        offset: 8,
        sort: Some(("label".into(), SortDir::Desc)),
        filter: Some(Filter {
            column: "label".into(),
            op: FilterOp::Contains,
            value: "row 1".into(),
        }),
        ..request("numbers")
    };
    assert_eq!(pool.count_rows(&req).await.unwrap(), 10);

    // Descending 19..10, offset 8 into the match set: the partial last page.
    let page = pool.fetch_page(&req).await.unwrap();
    assert_eq!(n_values(&page.rows), [11, 10]);

    req.offset = 0;
    let first = pool.fetch_page(&req).await.unwrap();
    assert_eq!(n_values(&first.rows), [19, 18, 17]);

    pool.close().await;
}

#[tokio::test]
async fn paging_works_on_tables_and_columns_with_weird_names() {
    let fixture = FixtureDb::full().await;
    let pool = fixture.open().await;

    // Table name with an embedded double quote and a space; sort on a column
    // with a space, filter on it too, and read a unicode-named column.
    let req = PageRequest {
        schema: None,
        table: "we\"ird table".into(),
        limit: 10,
        offset: 0,
        sort: Some(("col name".into(), SortDir::Desc)),
        filter: Some(Filter {
            column: "col name".into(),
            op: FilterOp::Contains,
            value: "row".into(),
        }),
        extra_key_column: None,
    };
    assert_eq!(pool.count_rows(&req).await.unwrap(), 3);
    let page = pool.fetch_page(&req).await.unwrap();
    let names: Vec<&str> = page.columns.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, ["col name", "übercol", "select"]);
    assert_eq!(page.rows[0][0], Value::Text("другой row".into()));
    assert_eq!(page.rows[0][1], Value::Real(2.5));

    // An SQL keyword as table name with a keyword column.
    let req = PageRequest {
        schema: None,
        filter: Some(Filter {
            column: "group".into(),
            op: FilterOp::Equals,
            value: "g1".into(),
        }),
        ..request("order")
    };
    let page = pool.fetch_page(&req).await.unwrap();
    assert_eq!(page.rows.len(), 1);
    assert_eq!(page.rows[0][0], Value::Integer(1));

    pool.close().await;
}

#[tokio::test]
async fn views_page_like_tables() {
    let fixture = FixtureDb::full().await;
    let pool = fixture.open().await;

    let req = sorted("artist_overview", "album_count", SortDir::Desc);
    assert_eq!(pool.count_rows(&req).await.unwrap(), 3);
    let page = pool.fetch_page(&req).await.unwrap();
    assert_eq!(page.rows.len(), 3);
    // Ana has two albums, Bo one, Cleo none.
    assert_eq!(page.rows[0][1], Value::Text("Ana".into()));
    assert_eq!(page.rows[0][2], Value::Integer(2));
    assert_eq!(page.rows[2][1], Value::Text("Cleo".into()));
    assert_eq!(page.rows[2][2], Value::Integer(0));

    pool.close().await;
}

#[tokio::test]
async fn fetched_page_round_trips_all_storage_classes() {
    let fixture = FixtureDb::full().await;
    let pool = fixture.open().await;

    let page = pool
        .fetch_page(&sorted("artists", "id", SortDir::Asc))
        .await
        .unwrap();
    assert_eq!(
        page.rows[0],
        vec![
            Value::Integer(1),
            Value::Text("Ana".into()),
            Value::Real(4.5),
            Value::Blob(vec![1, 2, 3]),
            Value::Null,
        ]
    );
    assert_eq!(page.rows[1][2], Value::Null);
    assert_eq!(page.rows[2][3], Value::Blob(vec![]));

    pool.close().await;
}
