//! Integration tests for bounded-memory reads (FRE-33) on SQLite:
//! `fetch_page_bounded` (truncated previews of large columns),
//! `fetch_cell` (lazy full-value load), and `query_capped` (row cap +
//! per-cell cap on the free-form query path).

mod common;

use common::FixtureDb;
use dataview::db::{
    detect_row_identity, Generated, PageRequest, RowLocator, Value, PREVIEW_BYTES, QUERY_CELL_CAP,
};

fn request(table: &str) -> PageRequest {
    PageRequest {
        schema: None,
        table: table.into(),
        limit: 100,
        offset: 0,
        sort: Some(("id".into(), dataview::db::SortDir::Asc)),
        filter: None,
        extra_key_column: None,
    }
}

/// Bytes a decoded value costs (the observable proxy for retained memory).
fn value_bytes(value: &Value) -> usize {
    match value {
        Value::Text(t) => t.len(),
        Value::Blob(b) => b.len(),
        _ => 8,
    }
}

/// A table with a short text column, a multi-KB text column, and a multi-KB
/// blob column — the shapes the preview path must bound.
async fn docs_fixture() -> FixtureDb {
    let big = "A".repeat(50_000);
    let sql = format!(
        "CREATE TABLE docs (
            id INTEGER PRIMARY KEY,
            small_note TEXT,
            big_text TEXT,
            payload BLOB
        );
        INSERT INTO docs (id, small_note, big_text, payload) VALUES
            (1, 'hi', '{big}', zeroblob(40000)),
            (2, 'yo', 'short enough', x'0102'),
            (3, 'nil', NULL, NULL);"
    );
    FixtureDb::with_sql(&sql).await
}

#[tokio::test]
async fn fetch_page_bounded_previews_large_columns_and_stays_small() {
    let fixture = docs_fixture().await;
    let pool = fixture.open().await;
    let tables = pool.introspect().await.unwrap();
    let docs = tables.iter().find(|t| t.name == "docs").unwrap();

    let page = pool
        .fetch_page_bounded(&request("docs"), &docs.columns, &["id"])
        .await
        .unwrap();

    // The trailing length helper columns are stripped: 4 visible columns.
    assert_eq!(page.result.columns.len(), 4);
    assert_eq!(page.result.rows.len(), 3);

    // Row 1's big_text is a bounded preview, flagged with the real length.
    let big_text = &page.result.rows[0][2];
    if let Value::Text(t) = big_text {
        assert!(
            t.chars().count() <= PREVIEW_BYTES,
            "preview length {} exceeds cap",
            t.chars().count()
        );
    } else {
        panic!("expected a text preview, got {big_text:?}");
    }
    let text_preview = page.previews[0][2].expect("big_text is truncated");
    assert_eq!(text_preview.full_len, 50_000);
    assert!(!text_preview.binary);

    // Row 1's blob is previewed too: the grid only shows its (real) size.
    let blob_preview = page.previews[0][3].expect("blob is truncated");
    assert_eq!(blob_preview.full_len, 40_000);
    assert!(blob_preview.binary);
    if let Value::Blob(b) = &page.result.rows[0][3] {
        assert!(b.len() <= PREVIEW_BYTES + 1, "blob prefix bounded");
    } else {
        panic!("expected a blob preview");
    }

    // Small columns and short values are never previewed.
    assert!(page.previews[0][1].is_none(), "short small_note");
    assert!(page.previews[1][2].is_none(), "short big_text");
    assert!(page.previews[0][0].is_none(), "scalar id");
    // NULLs carry no preview.
    assert!(page.previews[2][2].is_none());

    // The whole decoded page is a few KB — NOT the ~90 KB the full values hold.
    let total: usize = page
        .result
        .rows
        .iter()
        .flat_map(|r| r.iter())
        .map(value_bytes)
        .sum();
    let bound = page.result.rows.len() * page.result.columns.len() * (PREVIEW_BYTES + 64);
    assert!(
        total <= bound,
        "decoded page size {total} exceeds the bound {bound}"
    );

    pool.close().await;
}

#[tokio::test]
async fn bounded_page_keeps_generated_columns() {
    // SQLite `SELECT *` includes generated columns; the bounded projection is
    // built from metadata, so introspection must report them (FRE-33) — or
    // they would silently vanish from the grid.
    let sql = "CREATE TABLE gen (
                    a INTEGER PRIMARY KEY,
                    b TEXT,
                    c TEXT GENERATED ALWAYS AS (b || '!') STORED
                );
                INSERT INTO gen (a, b) VALUES (1, 'x');";
    let fixture = FixtureDb::with_sql(sql).await;
    let pool = fixture.open().await;
    let tables = pool.introspect().await.unwrap();
    let gen = tables.iter().find(|t| t.name == "gen").unwrap();

    let names: Vec<&str> = gen.columns.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, ["a", "b", "c"]);
    let c = gen.columns.iter().find(|c| c.name == "c").unwrap();
    assert_eq!(
        c.generated,
        Generated::Always,
        "generated column is read-only"
    );

    let page = pool
        .fetch_page_bounded(&request("gen"), &gen.columns, &["a"])
        .await
        .unwrap();
    let cols: Vec<&str> = page
        .result
        .columns
        .iter()
        .map(|c| c.name.as_str())
        .collect();
    assert_eq!(
        cols,
        ["a", "b", "c"],
        "generated column survives the projection"
    );
    assert_eq!(page.result.rows[0][2], Value::Text("x!".into()));

    pool.close().await;
}

#[tokio::test]
async fn fetch_cell_loads_the_full_value() {
    let fixture = docs_fixture().await;
    let pool = fixture.open().await;
    let tables = pool.introspect().await.unwrap();
    let docs = tables.iter().find(|t| t.name == "docs").unwrap();
    let identity = detect_row_identity(docs, pool.dialect()).unwrap();

    let locator = RowLocator {
        identity_values: vec![Value::Integer(1)],
    };
    let fetched = pool
        .fetch_cell(docs, &identity, &locator, "big_text")
        .await
        .unwrap();
    assert_eq!(fetched.full_len, 50_000);
    assert!(!fetched.capped);
    if let Value::Text(t) = &fetched.value {
        assert_eq!(t.chars().count(), 50_000, "the full value, not a preview");
    } else {
        panic!("expected full text");
    }

    // Safety anchor for edit correctness (FRE-33): the grid's page value for
    // this cell is a PREVIEW, strictly shorter than the full value — so the
    // editor must reload via fetch_cell before staging, never stage the page
    // value. Prove the two differ in length.
    let page = pool
        .fetch_page_bounded(&request("docs"), &docs.columns, &["id"])
        .await
        .unwrap();
    if let Value::Text(preview) = &page.result.rows[0][2] {
        assert!(
            preview.len() < 50_000,
            "the page holds a preview, not the full value"
        );
    } else {
        panic!("expected a preview");
    }

    pool.close().await;
}

#[tokio::test]
async fn fetch_cell_reports_a_missing_row_as_null() {
    let fixture = docs_fixture().await;
    let pool = fixture.open().await;
    let tables = pool.introspect().await.unwrap();
    let docs = tables.iter().find(|t| t.name == "docs").unwrap();
    let identity = detect_row_identity(docs, pool.dialect()).unwrap();

    let locator = RowLocator {
        identity_values: vec![Value::Integer(999)], // no such row
    };
    let fetched = pool
        .fetch_cell(docs, &identity, &locator, "big_text")
        .await
        .unwrap();
    assert_eq!(fetched.value, Value::Null);
    assert!(!fetched.capped);

    pool.close().await;
}

#[tokio::test]
async fn query_capped_stops_at_max_rows_and_flags_truncation() {
    let fixture = FixtureDb::numbers(50).await;
    let pool = fixture.open().await;

    // More rows than the cap: exactly `max_rows` retained, flagged truncated.
    let (result, truncated) = pool
        .query_capped("SELECT n FROM numbers ORDER BY n", &[], 10)
        .await
        .unwrap();
    assert_eq!(result.rows.len(), 10);
    assert!(truncated);
    assert_eq!(result.rows[0][0], Value::Integer(1));
    assert_eq!(result.rows[9][0], Value::Integer(10));

    // Exactly `max_rows` rows exist: full result, NOT flagged.
    let (result, truncated) = pool
        .query_capped("SELECT n FROM numbers ORDER BY n LIMIT 10", &[], 10)
        .await
        .unwrap();
    assert_eq!(result.rows.len(), 10);
    assert!(!truncated);

    // Fewer rows than the cap: everything, not flagged.
    let (result, truncated) = pool
        .query_capped("SELECT n FROM numbers WHERE n <= 5", &[], 100)
        .await
        .unwrap();
    assert_eq!(result.rows.len(), 5);
    assert!(!truncated);

    pool.close().await;
}

#[tokio::test]
async fn query_capped_bounds_huge_cells() {
    let big = "Z".repeat(200_000);
    let sql = format!("CREATE TABLE t (v TEXT); INSERT INTO t VALUES ('{big}');");
    let fixture = FixtureDb::with_sql(&sql).await;
    let pool = fixture.open().await;

    let (result, _truncated) = pool
        .query_capped("SELECT v FROM t", &[], 100)
        .await
        .unwrap();
    if let Value::Text(t) = &result.rows[0][0] {
        assert!(
            t.len() <= QUERY_CELL_CAP,
            "cell should be capped to {QUERY_CELL_CAP}, got {}",
            t.len()
        );
    } else {
        panic!("expected text");
    }

    pool.close().await;
}
