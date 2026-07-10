//! End-to-end tests for staged edits (FRE-14): `apply_staged` runs a whole
//! change list — updates, inserts, deletes — in ONE transaction on both
//! backends, rolling everything back when any change fails and naming the
//! failing change by index.
//!
//! Postgres cases need a running server (Docker only, per CLAUDE.md) and are
//! skipped unless `DATAVIEW_PG_TEST_URL` is set — see tests/db_postgres.rs
//! for the docker run recipe.

mod common;

use common::FixtureDb;
use dataview::db::{
    apply_staged, detect_row_identity, DbPool, Dialect, PageRequest, RowIdentity, RowLocator,
    StagedChange, TableMeta, Value,
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

fn find<'a>(tables: &'a [TableMeta], schema: Option<&str>, name: &str) -> &'a TableMeta {
    tables
        .iter()
        .find(|t| t.schema.as_deref() == schema && t.name == name)
        .unwrap_or_else(|| panic!("no table {schema:?}.{name}"))
}

fn locator(values: Vec<Value>) -> RowLocator {
    RowLocator {
        identity_values: values,
    }
}

fn update(locator_values: Vec<Value>, column: &str, value: Value) -> StagedChange {
    StagedChange::Update {
        locator: locator(locator_values),
        column: column.into(),
        value,
    }
}

#[tokio::test]
async fn sqlite_multi_change_batch_applies_atomically() {
    let fixture = FixtureDb::full().await;
    let pool = fixture.open().await;
    let tables = pool.introspect().await.unwrap();
    let albums = find(&tables, None, "albums").clone();
    let identity = detect_row_identity(&albums, Dialect::Sqlite).unwrap();

    // Two updates on different rows + one insert + one delete, one batch.
    let changes = vec![
        update(
            vec![Value::Integer(1), Value::Integer(1)],
            "title",
            Value::Text("First (remaster)".into()),
        ),
        update(
            vec![Value::Integer(1), Value::Integer(2)],
            "title",
            Value::Text("Second (remaster)".into()),
        ),
        StagedChange::Insert {
            columns: vec!["artist_id".into(), "seq".into(), "title".into()],
            values: vec![
                Value::Integer(2),
                Value::Integer(2),
                Value::Text("Encore".into()),
            ],
        },
        StagedChange::Delete {
            locator: locator(vec![Value::Integer(2), Value::Integer(1)]),
        },
    ];
    let counts = apply_staged(&pool, &albums, &identity, &changes)
        .await
        .unwrap();
    assert_eq!(counts.updated_rows, 2);
    assert_eq!(counts.inserted_rows, 1);
    assert_eq!(counts.deleted_rows, 1);

    let check = pool
        .query("SELECT artist_id, seq, title FROM albums ORDER BY artist_id, seq")
        .await
        .unwrap();
    let titles: Vec<&Value> = check.rows.iter().map(|r| &r[2]).collect();
    assert_eq!(
        titles,
        [
            &Value::Text("First (remaster)".into()),
            &Value::Text("Second (remaster)".into()),
            &Value::Text("Encore".into()),
        ]
    );

    pool.close().await;
}

#[tokio::test]
async fn sqlite_multi_column_edits_on_one_row_apply_as_one_row_update() {
    let fixture = FixtureDb::full().await;
    let pool = fixture.open().await;
    let tables = pool.introspect().await.unwrap();
    let artists = find(&tables, None, "artists").clone();
    let identity = detect_row_identity(&artists, Dialect::Sqlite).unwrap();

    // Three columns of one row, incl. a SET NULL rendered as literal NULL.
    let changes = vec![
        update(vec![Value::Integer(1)], "name", Value::Text("Ana B".into())),
        update(vec![Value::Integer(1)], "rating", Value::Null),
        update(
            vec![Value::Integer(1)],
            "notes",
            Value::Text("edited".into()),
        ),
    ];
    let counts = apply_staged(&pool, &artists, &identity, &changes)
        .await
        .unwrap();
    assert_eq!(counts.updated_rows, 1, "grouped into one row update");

    let check = pool
        .query("SELECT name, rating, notes FROM artists WHERE id = 1")
        .await
        .unwrap();
    assert_eq!(check.rows[0][0], Value::Text("Ana B".into()));
    assert_eq!(check.rows[0][1], Value::Null);
    assert_eq!(check.rows[0][2], Value::Text("edited".into()));

    pool.close().await;
}

#[tokio::test]
async fn sqlite_failure_mid_batch_rolls_everything_back_and_names_the_change() {
    let fixture = FixtureDb::full().await;
    let pool = fixture.open().await;
    let tables = pool.introspect().await.unwrap();
    let albums = find(&tables, None, "albums").clone();
    let identity = detect_row_identity(&albums, Dialect::Sqlite).unwrap();

    // First change is valid; second targets a row that does not exist
    // (0 rows affected), which must abort and roll back the first too.
    let changes = vec![
        update(
            vec![Value::Integer(1), Value::Integer(1)],
            "title",
            Value::Text("should not stick".into()),
        ),
        update(
            vec![Value::Integer(99), Value::Integer(99)],
            "title",
            Value::Text("nope".into()),
        ),
        StagedChange::Insert {
            columns: vec!["artist_id".into(), "seq".into(), "title".into()],
            values: vec![
                Value::Integer(3),
                Value::Integer(1),
                Value::Text("also not".into()),
            ],
        },
    ];
    let err = apply_staged(&pool, &albums, &identity, &changes)
        .await
        .unwrap_err();
    assert_eq!(err.change_index, Some(1), "the second change failed");
    assert_eq!(
        err.change_summary,
        Some("update of row (99, 99) [columns title]".into())
    );
    assert!(
        err.message.contains("affected 0 rows"),
        "got: {}",
        err.message
    );
    assert!(err.message.contains("rolled back"), "got: {}", err.message);

    // Nothing landed — not even the valid first update or the insert.
    let check = pool
        .query("SELECT COUNT(*) FROM albums WHERE title IN ('should not stick', 'also not')")
        .await
        .unwrap();
    assert_eq!(check.rows[0][0], Value::Integer(0));
    let count = pool.query("SELECT COUNT(*) FROM albums").await.unwrap();
    assert_eq!(count.rows[0][0], Value::Integer(3));

    pool.close().await;
}

#[tokio::test]
async fn sqlite_sql_error_mid_batch_rolls_back_and_names_the_change() {
    let fixture = FixtureDb::full().await;
    let pool = fixture.open().await;
    let tables = pool.introspect().await.unwrap();
    let albums = find(&tables, None, "albums").clone();
    let identity = detect_row_identity(&albums, Dialect::Sqlite).unwrap();

    // The insert violates the primary key (1, 1 already exists): a real SQL
    // error, not a count mismatch.
    let changes = vec![
        update(
            vec![Value::Integer(1), Value::Integer(1)],
            "title",
            Value::Text("should not stick".into()),
        ),
        StagedChange::Insert {
            columns: vec!["artist_id".into(), "seq".into(), "title".into()],
            values: vec![
                Value::Integer(1),
                Value::Integer(1),
                Value::Text("dup".into()),
            ],
        },
    ];
    let err = apply_staged(&pool, &albums, &identity, &changes)
        .await
        .unwrap_err();
    assert_eq!(err.change_index, Some(1));

    let check = pool
        .query("SELECT title FROM albums WHERE artist_id = 1 AND seq = 1")
        .await
        .unwrap();
    assert_eq!(check.rows[0][0], Value::Text("First".into()));

    pool.close().await;
}

#[tokio::test]
async fn sqlite_rowid_identity_table_edits_end_to_end() {
    // A keyless table: identity is the implicit rowid, which `SELECT *`
    // does not include — the page fetch must be asked for it.
    let fixture = FixtureDb::with_sql(
        "CREATE TABLE notes (body TEXT);
         INSERT INTO notes (body) VALUES ('same'), ('same'), ('other');",
    )
    .await;
    let pool = fixture.open().await;
    let tables = pool.introspect().await.unwrap();
    let notes = find(&tables, None, "notes").clone();
    let identity = detect_row_identity(&notes, Dialect::Sqlite).unwrap();
    let RowIdentity::Rowid { column } = &identity else {
        panic!("expected rowid identity, got {identity:?}");
    };

    // Fetch a page exactly as the grid does for rowid tables: the key
    // column is prepended to the row and hidden from display.
    let request = PageRequest {
        schema: None,
        table: "notes".into(),
        limit: 10,
        offset: 0,
        sort: None,
        filter: None,
        extra_key_column: Some(column.clone()),
    };
    let page = pool.fetch_page(&request).await.unwrap();
    assert_eq!(page.columns[0].name, "rowid");
    assert_eq!(page.columns[1].name, "body");
    assert_eq!(page.rows.len(), 3);

    // Two rows are identical in every user column; the fetched rowid still
    // addresses exactly one. Edit the second 'same' row via its locator.
    let target = page
        .rows
        .iter()
        .filter(|row| row[1] == Value::Text("same".into()))
        .nth(1)
        .expect("two 'same' rows")
        .clone();
    let changes = vec![update(
        vec![target[0].clone()],
        "body",
        Value::Text("edited".into()),
    )];
    let counts = apply_staged(&pool, &notes, &identity, &changes)
        .await
        .unwrap();
    assert_eq!(counts.updated_rows, 1);

    let check = pool
        .query("SELECT body FROM notes ORDER BY rowid")
        .await
        .unwrap();
    let bodies: Vec<&Value> = check.rows.iter().map(|r| &r[0]).collect();
    assert_eq!(
        bodies,
        [
            &Value::Text("same".into()),
            &Value::Text("edited".into()),
            &Value::Text("other".into()),
        ]
    );

    pool.close().await;
}

/// Drops and recreates a postgres test schema with one table
/// `<schema>.items (id integer PK, label text NOT NULL, quantity integer)`.
async fn pg_fixture(pool: &DbPool, schema: &str) {
    for sql in [
        &format!("DROP SCHEMA IF EXISTS {schema} CASCADE"),
        &format!("CREATE SCHEMA {schema}"),
        &format!(
            "CREATE TABLE {schema}.items (
                id integer PRIMARY KEY,
                label text NOT NULL,
                quantity integer
            )"
        ),
        &format!(
            "INSERT INTO {schema}.items VALUES
                (1, 'one', 10), (2, 'two', 20), (3, 'three', 30)"
        ),
    ] {
        pool.query(sql).await.unwrap();
    }
}

#[tokio::test]
async fn postgres_multi_change_batch_applies_atomically() {
    let Some(url) = test_url() else { return };
    let pool = DbPool::open_postgres(&url).await.unwrap();
    pg_fixture(&pool, "staged_batch").await;
    let tables = pool.introspect().await.unwrap();
    let items = find(&tables, Some("staged_batch"), "items").clone();
    let identity = detect_row_identity(&items, Dialect::Postgres).unwrap();

    let changes = vec![
        update(vec![Value::Integer(1)], "label", Value::Text("uno".into())),
        update(vec![Value::Integer(2)], "quantity", Value::Integer(22)),
        StagedChange::Insert {
            columns: vec!["id".into(), "label".into(), "quantity".into()],
            values: vec![Value::Integer(4), Value::Text("four".into()), Value::Null],
        },
        StagedChange::Delete {
            locator: locator(vec![Value::Integer(3)]),
        },
    ];
    let counts = apply_staged(&pool, &items, &identity, &changes)
        .await
        .unwrap();
    assert_eq!(counts.updated_rows, 2);
    assert_eq!(counts.inserted_rows, 1);
    assert_eq!(counts.deleted_rows, 1);

    let check = pool
        .query("SELECT id, label, quantity FROM staged_batch.items ORDER BY id")
        .await
        .unwrap();
    assert_eq!(check.rows.len(), 3);
    assert_eq!(check.rows[0][1], Value::Text("uno".into()));
    assert_eq!(check.rows[1][2], Value::Integer(22));
    assert_eq!(check.rows[2][0], Value::Integer(4));
    // The insert's NULL for an integer column landed as SQL NULL (rendered
    // as a literal, not bound as a text-typed NULL).
    assert_eq!(check.rows[2][2], Value::Null);

    pool.close().await;
}

#[tokio::test]
async fn postgres_failure_mid_batch_rolls_everything_back_and_names_the_change() {
    let Some(url) = test_url() else { return };
    let pool = DbPool::open_postgres(&url).await.unwrap();
    pg_fixture(&pool, "staged_rollback").await;
    let tables = pool.introspect().await.unwrap();
    let items = find(&tables, Some("staged_rollback"), "items").clone();
    let identity = detect_row_identity(&items, Dialect::Postgres).unwrap();

    let changes = vec![
        update(
            vec![Value::Integer(1)],
            "label",
            Value::Text("stick?".into()),
        ),
        // Targets no row: 0 affected, expected 1 → whole batch rolls back.
        StagedChange::Delete {
            locator: locator(vec![Value::Integer(99)]),
        },
    ];
    let err = apply_staged(&pool, &items, &identity, &changes)
        .await
        .unwrap_err();
    assert_eq!(err.change_index, Some(1));
    assert_eq!(err.change_summary, Some("delete of row (99)".into()));
    assert!(
        err.message.contains("affected 0 rows"),
        "got: {}",
        err.message
    );

    let check = pool
        .query("SELECT label FROM staged_rollback.items WHERE id = 1")
        .await
        .unwrap();
    assert_eq!(check.rows[0][0], Value::Text("one".into()));

    pool.close().await;
}

#[tokio::test]
async fn postgres_set_null_on_integer_column_works_via_literal_null() {
    let Some(url) = test_url() else { return };
    let pool = DbPool::open_postgres(&url).await.unwrap();
    pg_fixture(&pool, "staged_null").await;
    let tables = pool.introspect().await.unwrap();
    let items = find(&tables, Some("staged_null"), "items").clone();
    let identity = detect_row_identity(&items, Dialect::Postgres).unwrap();

    // The motivating case for literal-NULL rendering: a bound Value::Null is
    // a NULL of type text, which Postgres rejects for `SET quantity = $n`
    // on an integer column. Rendered as literal NULL it must succeed —
    // together with a non-NULL edit of another column in the same UPDATE.
    let changes = vec![
        update(vec![Value::Integer(1)], "quantity", Value::Null),
        update(
            vec![Value::Integer(1)],
            "label",
            Value::Text("emptied".into()),
        ),
    ];
    let counts = apply_staged(&pool, &items, &identity, &changes)
        .await
        .unwrap();
    assert_eq!(counts.updated_rows, 1);

    let check = pool
        .query("SELECT label, quantity FROM staged_null.items WHERE id = 1")
        .await
        .unwrap();
    assert_eq!(check.rows[0][0], Value::Text("emptied".into()));
    assert_eq!(check.rows[0][1], Value::Null);

    pool.close().await;
}
