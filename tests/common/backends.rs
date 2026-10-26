//! Assertions the SQLite, Postgres and SQL Server suites make the same way.
//!
//! Each backend's suite used to write these scenarios out for itself, so a
//! claim tightened in one copy stayed loose in the other two. Here each claim
//! is stated once; the suites supply what differs per backend (the SQL, the
//! fixture, how a table is introspected) and keep the checks only they make.
//!
//! Nothing here introspects (`tests/engine_urls.rs` pins that for all of
//! `tests/common`): an assertion that needs a table's metadata is handed it.

use hubro::db::{
    detect_row_identity, DbError, DbPool, Page, PageRequest, Reading, RowLocator, TableMeta, Value,
    MISSING_ROW, PREVIEW_BYTES, QUERY_CELL_CAP,
};

// ---------------------------------------------------------------------------
// The capped query path
// ---------------------------------------------------------------------------

/// The SQL [`assert_query_capped`] runs, in the backend's own dialect.
pub struct CappedQueries {
    /// One column holding 1 to 25, in that order.
    pub one_to_25: &'static str,
    /// One column holding 1 to 10.
    pub exactly_ten: &'static str,
    /// One column holding five rows.
    pub five: &'static str,
    /// One row whose one text cell is 200 000 characters long.
    pub huge_cell: &'static str,
}

/// `query_capped` keeps exactly `max_rows` rows of a longer result and says
/// it cut it short; a result of exactly `max_rows` rows, or fewer, comes back
/// whole and is not flagged; and a cell far past [`QUERY_CELL_CAP`] is cut
/// to it.
pub async fn assert_query_capped(pool: &DbPool, queries: &CappedQueries) {
    let (result, truncated) = pool.query_capped(queries.one_to_25, &[], 10).await.unwrap();
    assert_eq!(result.rows.len(), 10, "more rows than the cap: the cap");
    assert!(truncated, "more rows than the cap must be flagged");
    assert_eq!(result.rows[0][0], Value::Integer(1));
    assert_eq!(result.rows[9][0], Value::Integer(10));

    let (result, truncated) = pool
        .query_capped(queries.exactly_ten, &[], 10)
        .await
        .unwrap();
    assert_eq!(result.rows.len(), 10);
    assert!(!truncated, "exactly the cap is the whole result");

    let (result, truncated) = pool.query_capped(queries.five, &[], 100).await.unwrap();
    assert_eq!(result.rows.len(), 5);
    assert!(!truncated, "under the cap is the whole result");

    let (result, _) = pool.query_capped(queries.huge_cell, &[], 10).await.unwrap();
    match &result.rows[0][0] {
        Value::Text(t) => assert!(
            t.len() <= QUERY_CELL_CAP,
            "cell should be capped to {QUERY_CELL_CAP}, got {}",
            t.len()
        ),
        other => panic!("expected text, got {other:?}"),
    }
}

/// A read that matches no rows still names its columns, `columns`, on both
/// the plain and the capped path: the grid shows the shape of what was asked
/// for rather than a blank pane.
pub async fn assert_empty_result_keeps_headers(pool: &DbPool, sql: &str, columns: &[&str]) {
    let names = |result: &hubro::db::QueryResult| -> Vec<String> {
        result.columns.iter().map(|c| c.name.clone()).collect()
    };
    let empty = pool.query_unbounded(sql).await.unwrap();
    assert!(empty.rows.is_empty(), "{sql} should match no rows");
    assert_eq!(names(&empty), columns, "the plain path");

    let (empty, truncated) = pool.query_capped(sql, &[], 100).await.unwrap();
    assert!(empty.rows.is_empty());
    assert!(!truncated);
    assert_eq!(names(&empty), columns, "the capped path");
}

/// `statement`, which has no result set, runs through the capped path as an
/// ordinary empty result with no columns, not as an error. The header
/// fallback describes the statement, and a `DROP TABLE` is the sharp case: it
/// cannot be described again once it has run.
pub async fn assert_no_result_set_is_no_columns(pool: &DbPool, statement: &str) {
    let (result, truncated) = pool
        .query_capped(statement, &[], 100)
        .await
        .unwrap_or_else(|e| panic!("{statement}: {e}"));
    assert!(result.rows.is_empty());
    assert!(result.columns.is_empty());
    assert!(!truncated);
}

// ---------------------------------------------------------------------------
// Bounded pages and full cells
// ---------------------------------------------------------------------------

/// How long the docs fixture's large values are: row 1's `big_text` in
/// characters and its `payload` in bytes.
pub const BIG_TEXT_CHARS: u64 = 50_000;
pub const PAYLOAD_BYTES: u64 = 40_000;

/// `docs` is the docs fixture each backend builds in its own dialect:
/// `id` (integer primary key), `small_note`, `big_text` and `payload`
/// (binary), in that order. Row 1 is `'hi'`, [`BIG_TEXT_CHARS`] characters
/// and [`PAYLOAD_BYTES`] bytes; row 2 is `'yo'`, `'short enough'` and two
/// bytes.
///
/// A bounded page of it, read by `request` in `id` order, strips the length
/// helper columns, holds row 1's large values as previews no longer than
/// [`PREVIEW_BYTES`] that carry their real length and kind, and previews
/// nothing short. Returns the page, for the checks only one backend makes.
pub async fn assert_bounded_page_previews(
    pool: &DbPool,
    docs: &TableMeta,
    request: &PageRequest,
) -> Page {
    let page = pool
        .fetch_page_bounded(Reading::UNMARKED, request, &docs.columns, &["id"])
        .await
        .unwrap();
    assert_eq!(page.result.columns.len(), 4, "length columns stripped");

    let text_preview = page.previews[0][2].expect("big_text truncated");
    assert_eq!(text_preview.full_len, BIG_TEXT_CHARS);
    assert!(!text_preview.binary);
    match &page.result.rows[0][2] {
        Value::Text(t) => assert!(
            t.chars().count() <= PREVIEW_BYTES,
            "preview length {} exceeds the cap",
            t.chars().count()
        ),
        other => panic!("expected a text preview, got {other:?}"),
    }

    let blob_preview = page.previews[0][3].expect("payload truncated");
    assert_eq!(blob_preview.full_len, PAYLOAD_BYTES);
    assert!(blob_preview.binary);

    // Short values, and the short column, are complete.
    assert!(page.previews[0][0].is_none(), "scalar id");
    assert!(page.previews[0][1].is_none(), "short small_note");
    assert!(page.previews[1][2].is_none(), "short big_text");
    assert!(page.previews[1][3].is_none(), "short payload");
    page
}

/// `fetch_cell` loads row 1 of the docs fixture's large values in full:
/// every character of `big_text` and every byte of `payload`, neither capped.
pub async fn assert_large_cells_load_in_full(pool: &DbPool, docs: &TableMeta) {
    let identity = detect_row_identity(docs, pool.dialect()).unwrap();
    let row_one = RowLocator {
        identity_values: vec![Value::Integer(1)],
    };
    let cell = pool
        .fetch_cell(Reading::UNMARKED, docs, &identity, &row_one, "big_text")
        .await
        .unwrap();
    assert_eq!(cell.full_len, BIG_TEXT_CHARS);
    assert!(!cell.capped);
    match &cell.value {
        Value::Text(t) => assert_eq!(t.chars().count() as u64, BIG_TEXT_CHARS),
        other => panic!("expected the full text, got {other:?}"),
    }
    let cell = pool
        .fetch_cell(Reading::UNMARKED, docs, &identity, &row_one, "payload")
        .await
        .unwrap();
    assert_eq!(cell.full_len, PAYLOAD_BYTES);
    assert!(!cell.capped);
    match &cell.value {
        Value::Blob(b) => assert_eq!(b.len() as u64, PAYLOAD_BYTES),
        other => panic!("expected the full blob, got {other:?}"),
    }
}

/// A cell of row `id`, which the caller has deleted underneath the grid,
/// fails to load with [`MISSING_ROW`] rather than loading as NULL: a NULL
/// would open the editor on a value that is not there.
pub async fn assert_a_deleted_rows_cell_does_not_load(pool: &DbPool, docs: &TableMeta, id: i64) {
    let identity = detect_row_identity(docs, pool.dialect()).unwrap();
    let locator = RowLocator {
        identity_values: vec![Value::Integer(id)],
    };
    match pool
        .fetch_cell(Reading::UNMARKED, docs, &identity, &locator, "big_text")
        .await
    {
        Err(DbError::Query(message)) => assert_eq!(message, MISSING_ROW),
        other => panic!("row {id} is gone, so its cell must not load: {other:?}"),
    }
}
