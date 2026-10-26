//! Integration tests for bounded-memory reads on SQLite:
//! `fetch_page_bounded` (truncated previews of large columns),
//! `fetch_cell` (lazy full-value load), and `query_capped` (row cap +
//! per-cell cap on the free-form query path).

mod common;

use common::backends::{
    assert_a_deleted_rows_cell_does_not_load, assert_bounded_page_previews,
    assert_large_cells_load_in_full, assert_query_capped, CappedQueries,
};

use common::FixtureDb;
use hubro::db::Reading;
use hubro::db::{
    detect_row_identity, DbError, Generated, PageRequest, RowLocator, TableMeta, Value,
    PREVIEW_BYTES,
};

fn request(table: &str) -> PageRequest {
    PageRequest {
        sort: Some(("id".into(), hubro::db::SortDir::Asc)),
        ..common::page(table)
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

    let page = assert_bounded_page_previews(&pool, docs, &request("docs")).await;
    assert_eq!(page.result.rows.len(), 3);

    // Row 1's blob prefix is bounded too: the grid only shows its (real) size.
    if let Value::Blob(b) = &page.result.rows[0][3] {
        assert!(b.len() <= PREVIEW_BYTES + 1, "blob prefix bounded");
    } else {
        panic!("expected a blob preview");
    }
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
    // built from metadata, so introspection must report them — or
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
        .fetch_page_bounded(Reading::UNMARKED, &request("gen"), &gen.columns, &["a"])
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
    assert_large_cells_load_in_full(&pool, docs).await;

    // Safety anchor for edit correctness: the grid's page value for
    // this cell is a PREVIEW, strictly shorter than the full value — so the
    // editor must reload via fetch_cell before staging, never stage the page
    // value. Prove the two differ in length.
    let page = pool
        .fetch_page_bounded(Reading::UNMARKED, &request("docs"), &docs.columns, &["id"])
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
async fn fetch_cell_reports_a_missing_row_as_an_error_not_null() {
    // A row deleted underneath the grid used to load as an empty value, so
    // the expand popup showed NULL, the truncated-cell editor opened on NULL
    // (and could stage a write from it) and the clipboard got an empty cell —
    // a failure that read as data. It is an error now, and a stored NULL is
    // still a value.
    let fixture = docs_fixture().await;
    let pool = fixture.open().await;
    let tables = pool.introspect().await.unwrap();
    let docs = tables.iter().find(|t| t.name == "docs").unwrap();
    let identity = detect_row_identity(docs, pool.dialect()).unwrap();
    let at = |id| RowLocator {
        identity_values: vec![Value::Integer(id)],
    };

    let stored_null = pool
        .fetch_cell(Reading::UNMARKED, docs, &identity, &at(3), "big_text")
        .await
        .unwrap();
    assert_eq!(stored_null.value, Value::Null);

    pool.query_unbounded("DELETE FROM docs WHERE id = 1")
        .await
        .unwrap();
    for id in [1, 999] {
        assert_a_deleted_rows_cell_does_not_load(&pool, docs, id).await;
    }

    pool.close().await;
}

#[tokio::test]
async fn query_capped_stops_at_max_rows_and_bounds_huge_cells() {
    let fixture = FixtureDb::numbers(50).await;
    let pool = fixture.open().await;
    assert_query_capped(
        &pool,
        &CappedQueries {
            one_to_25: "SELECT n FROM numbers WHERE n <= 25 ORDER BY n",
            exactly_ten: "SELECT n FROM numbers ORDER BY n LIMIT 10",
            five: "SELECT n FROM numbers WHERE n <= 5",
            // hex() of 100 000 zero bytes is 200 000 characters of text.
            huge_cell: "SELECT hex(zeroblob(100000))",
        },
    )
    .await;
    pool.close().await;
}

/// One table per key storage class SQLite has — `k_real`, `k_blob`,
/// `k_text` and a composite `k_composite` — each with a long `body`, and
/// beside each an `fk_<slug>` whose rows reference every key through a
/// foreign key, with `expect` naming the body each one must land on.
const KEY_TYPE_SLUGS: &[&str] = &["real", "blob", "text", "composite"];

async fn key_type_fixture() -> FixtureDb {
    let body = |n: usize| format!("'{n}:' || printf('%.5000c', 'x')");
    let sql = format!(
        "CREATE TABLE k_real (k REAL PRIMARY KEY, body TEXT, tag TEXT);
         INSERT INTO k_real VALUES (1e20, {}, 't'), (0.1, {}, 't'), (-2.5e-300, {}, 't');
         CREATE TABLE fk_real (ref REAL REFERENCES k_real (k), expect TEXT);
         INSERT INTO fk_real VALUES (1e20, '0:'), (0.1, '1:'), (-2.5e-300, '2:');
         CREATE TABLE k_blob (k BLOB PRIMARY KEY, body TEXT, tag TEXT);
         INSERT INTO k_blob VALUES (x'00ff10', {}, 't'), (x'', {}, 't');
         CREATE TABLE fk_blob (ref BLOB REFERENCES k_blob (k), expect TEXT);
         INSERT INTO fk_blob VALUES (x'00ff10', '0:'), (x'', '1:');
         CREATE TABLE k_text (k TEXT PRIMARY KEY, body TEXT, tag TEXT);
         INSERT INTO k_text VALUES ('plain', {}, 't'), ('trailing  ', {}, 't');
         CREATE TABLE fk_text (ref TEXT REFERENCES k_text (k), expect TEXT);
         INSERT INTO fk_text VALUES ('plain', '0:'), ('trailing  ', '1:');
         CREATE TABLE k_composite (a REAL, b TEXT, body TEXT, tag TEXT, PRIMARY KEY (a, b));
         INSERT INTO k_composite VALUES (1e20, 'x', {}, 't'), (1e20, 'y', {}, 't');
         CREATE TABLE fk_composite (a REAL, b TEXT, expect TEXT,
             FOREIGN KEY (a, b) REFERENCES k_composite (a, b));
         INSERT INTO fk_composite VALUES (1e20, 'x', '0:'), (1e20, 'y', '1:');",
        body(0),
        body(1),
        body(2),
        body(0),
        body(1),
        body(0),
        body(1),
        body(0),
        body(1),
    );
    FixtureDb::with_sql(&sql).await
}

/// Each `k_<slug>` table with its `fk_<slug>` referencing table.
fn key_type_pairs(tables: &[TableMeta]) -> Vec<(&TableMeta, &TableMeta)> {
    assert_eq!(tables.len(), KEY_TYPE_SLUGS.len() * 2);
    let named = |name: String| {
        tables
            .iter()
            .find(|t| t.name == name)
            .unwrap_or_else(|| panic!("{name} was not introspected"))
    };
    KEY_TYPE_SLUGS
        .iter()
        .map(|slug| (named(format!("k_{slug}")), named(format!("fk_{slug}"))))
        .collect()
}

#[tokio::test]
async fn every_key_type_reads_and_writes_the_same_row() {
    // The expand popup, the truncated-cell editor and the clipboard load a
    // long cell's full value through `fetch_cell`; Save re-addresses the row
    // through the staged writer. Both start from the same locator, so both
    // must find the same row for every key type — a blob key, bound as the
    // text `<blob N>`, used to expand to NULL while it saved.
    let fixture = key_type_fixture().await;
    let pool = fixture.open().await;
    let tables = pool.introspect().await.unwrap();
    let mut failures = Vec::new();
    for (table, _) in key_type_pairs(&tables) {
        failures.extend(common::row_addressing_failures(&pool, table).await);
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
    pool.close().await;
}

#[tokio::test]
async fn every_key_type_jumps_to_the_row_it_references() {
    // A foreign-key jump pins the referenced row from the source row's typed
    // values. Compared as text, a blob key was bound as the marker
    // `<blob N>` and jumped to an empty page.
    let fixture = key_type_fixture().await;
    let pool = fixture.open().await;
    let tables = pool.introspect().await.unwrap();
    let mut failures = Vec::new();
    for (parent, child) in key_type_pairs(&tables) {
        failures.extend(common::fk_jump_failures(&pool, child, parent).await);
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
    pool.close().await;
}

#[tokio::test]
async fn fetch_cell_refuses_a_locator_that_does_not_fit_the_key() {
    // A locator built against an older identity (the key gained or lost a
    // column after a re-introspection) used to pair up with the key by
    // position: one value against `albums (artist_id, seq)` became
    // `WHERE artist_id = 1`, and the first of that artist's albums came back
    // as if it were the row asked for — shown, and editable.
    let fixture = FixtureDb::full().await;
    let pool = fixture.open().await;
    let tables = pool.introspect().await.unwrap();
    let albums = tables.iter().find(|t| t.name == "albums").unwrap();
    let identity = detect_row_identity(albums, pool.dialect()).unwrap();
    assert_eq!(identity.key_columns(), ["artist_id", "seq"]);

    for values in [
        vec![Value::Integer(1)],
        vec![Value::Integer(1), Value::Integer(2), Value::Integer(3)],
    ] {
        let locator = RowLocator {
            identity_values: values,
        };
        match pool
            .fetch_cell(Reading::UNMARKED, albums, &identity, &locator, "title")
            .await
        {
            Err(DbError::Query(message)) => {
                assert!(message.contains("reload"), "{message}")
            }
            other => panic!("{locator:?} must be refused, got {other:?}"),
        }
    }
    // The full key still reads its row.
    let locator = RowLocator {
        identity_values: vec![Value::Integer(1), Value::Integer(2)],
    };
    let cell = pool
        .fetch_cell(Reading::UNMARKED, albums, &identity, &locator, "title")
        .await
        .unwrap();
    assert_eq!(cell.value, Value::Text("Second".into()));

    pool.close().await;
}
