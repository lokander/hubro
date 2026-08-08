//! End-to-end tests for staged edits (FRE-14): `apply_staged` runs a whole
//! change list — updates, inserts, deletes — in ONE transaction on both
//! backends, rolling everything back when any change fails and naming the
//! failing change by index. The FRE-25 cases drive the same path through
//! `TableStage` (per-column insert overrides, multi-delete with an edit
//! alongside) the way the grid stages them.
//!
//! Postgres cases need a running server (Docker only, per CLAUDE.md) and are
//! skipped unless `HUBRO_PG_TEST_URL` is set — see tests/db_postgres.rs
//! for the docker run recipe.

mod common;

use std::collections::HashSet;

use common::FixtureDb;
use hubro::db::{
    apply_staged, detect_row_identity, run_script, DbPool, Dialect, PageRequest, RowIdentity,
    RowLocator, StagedChange, TableAccess, TableMeta, Value, WriteProtection,
};
use hubro::ui::editing::bool_value;
use hubro::ui::stage::required_insert_columns;
use hubro::ui::TableStage;

fn test_url() -> Option<String> {
    match std::env::var("HUBRO_PG_TEST_URL") {
        Ok(url) => Some(url),
        Err(_) => {
            eprintln!("skipping postgres test: HUBRO_PG_TEST_URL not set");
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
    let counts = apply_staged(
        &pool,
        &pool.backend_access(&albums),
        &albums,
        &identity,
        &changes,
    )
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
    let counts = apply_staged(
        &pool,
        &pool.backend_access(&artists),
        &artists,
        &identity,
        &changes,
    )
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
    let err = apply_staged(
        &pool,
        &pool.backend_access(&albums),
        &albums,
        &identity,
        &changes,
    )
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
    let err = apply_staged(
        &pool,
        &pool.backend_access(&albums),
        &albums,
        &identity,
        &changes,
    )
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
    let counts = apply_staged(
        &pool,
        &pool.backend_access(&notes),
        &notes,
        &identity,
        &changes,
    )
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
    let counts = apply_staged(
        &pool,
        &pool.backend_access(&items),
        &items,
        &identity,
        &changes,
    )
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
    let err = apply_staged(
        &pool,
        &pool.backend_access(&items),
        &items,
        &identity,
        &changes,
    )
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
async fn sqlite_bool_checkbox_stages_integers_end_to_end() {
    // The checkbox editor stages Integer(0/1) on SQLite (bool_value); the
    // column's numeric affinity stores them as-is.
    let fixture = FixtureDb::with_sql(
        "CREATE TABLE flags (id INTEGER PRIMARY KEY, ok BOOLEAN);
         INSERT INTO flags VALUES (1, 0), (2, 1);",
    )
    .await;
    let pool = fixture.open().await;
    let tables = pool.introspect().await.unwrap();
    let flags = find(&tables, None, "flags").clone();
    let identity = detect_row_identity(&flags, Dialect::Sqlite).unwrap();

    let changes = vec![
        update(
            vec![Value::Integer(1)],
            "ok",
            bool_value(Dialect::Sqlite, true),
        ),
        update(
            vec![Value::Integer(2)],
            "ok",
            bool_value(Dialect::Sqlite, false),
        ),
    ];
    apply_staged(
        &pool,
        &pool.backend_access(&flags),
        &flags,
        &identity,
        &changes,
    )
    .await
    .unwrap();

    let check = pool
        .query("SELECT ok FROM flags ORDER BY id")
        .await
        .unwrap();
    assert_eq!(check.rows[0][0], Value::Integer(1));
    assert_eq!(check.rows[1][0], Value::Integer(0));

    pool.close().await;
}

/// The FRE-24 coercion path: the editor stages every rich Postgres value as
/// text (that's how FRE-12 renders them), and the staged SQL builder casts
/// each bound parameter to its column's introspected type
/// (`SET "col" = $n::integer`). Without the casts every one of these
/// updates fails the bind-type check (documented on postgres::bind_params).
#[tokio::test]
async fn postgres_staged_text_values_coerce_to_column_types() {
    let Some(url) = test_url() else { return };
    let pool = DbPool::open_postgres(&url).await.unwrap();
    for sql in [
        "DROP SCHEMA IF EXISTS staged_casts CASCADE",
        "CREATE SCHEMA staged_casts",
        "CREATE TABLE staged_casts.typed (
            id integer PRIMARY KEY,
            flag boolean,
            at timestamp,
            doc jsonb,
            amount numeric,
            quantity integer
        )",
        "INSERT INTO staged_casts.typed VALUES
            (1, false, '2000-01-01 00:00:00', '{}', 0, 0)",
    ] {
        pool.query(sql).await.unwrap();
    }
    let tables = pool.introspect().await.unwrap();
    let typed = find(&tables, Some("staged_casts"), "typed").clone();
    let identity = detect_row_identity(&typed, Dialect::Postgres).unwrap();

    let key = vec![Value::Integer(1)];
    let changes = vec![
        update(key.clone(), "flag", bool_value(Dialect::Postgres, true)),
        update(key.clone(), "at", Value::Text("2024-06-01 12:30:45".into())),
        update(
            key.clone(),
            "doc",
            Value::Text("{\"a\": 1, \"b\": [true, null]}".into()),
        ),
        // Exact numeric beyond f64 precision, staged as text by the editor.
        update(
            key.clone(),
            "amount",
            Value::Text("12345678901234567890.123456789".into()),
        ),
        // Integer columns coerce from text too (numeric input normally
        // stages Integer, but text must also survive the cast).
        update(key.clone(), "quantity", Value::Text("42".into())),
    ];
    let counts = apply_staged(
        &pool,
        &pool.backend_access(&typed),
        &typed,
        &identity,
        &changes,
    )
    .await
    .unwrap();
    assert_eq!(counts.updated_rows, 1);

    let check = pool
        .query("SELECT flag, at, doc, amount, quantity FROM staged_casts.typed WHERE id = 1")
        .await
        .unwrap();
    assert_eq!(check.rows[0][0], Value::Text("true".into()));
    assert_eq!(check.rows[0][1], Value::Text("2024-06-01 12:30:45".into()));
    // jsonb normalizes; the app renders JSON compactly on read-back.
    assert_eq!(
        check.rows[0][2],
        Value::Text("{\"a\":1,\"b\":[true,null]}".into())
    );
    assert_eq!(
        check.rows[0][3],
        Value::Text("12345678901234567890.123456789".into())
    );
    assert_eq!(check.rows[0][4], Value::Integer(42));

    // Bad text for a typed column fails at save time (the cast reports it)
    // and rolls back.
    let bad = vec![update(key, "at", Value::Text("not a date".into()))];
    let err = apply_staged(&pool, &pool.backend_access(&typed), &typed, &identity, &bad)
        .await
        .unwrap_err();
    assert_eq!(err.change_index, Some(0));

    pool.close().await;
}

/// Regression for the char(n) truncation bug: `information_schema` reports
/// a `character(3)` column as just "character", and `$1::character` means
/// char(1) — the cast would silently truncate "xyz" to 'x' on SET, and a
/// char(n) key column cast in the WHERE clause would never match its row
/// (aborting every save). The builder must leave these params uncast;
/// Postgres's assignment/comparison coercion then applies the column's
/// true modifier.
#[tokio::test]
async fn postgres_char_n_columns_round_trip_uncast() {
    let Some(url) = test_url() else { return };
    let pool = DbPool::open_postgres(&url).await.unwrap();
    for sql in [
        "DROP SCHEMA IF EXISTS staged_charn CASCADE",
        "CREATE SCHEMA staged_charn",
        // char(3) both as a plain column AND as the identity key.
        "CREATE TABLE staged_charn.items (
            code character(3) PRIMARY KEY,
            label text NOT NULL,
            tag character(3)
        )",
        "INSERT INTO staged_charn.items VALUES ('abc', 'one', 'aaa')",
    ] {
        pool.query(sql).await.unwrap();
    }
    let tables = pool.introspect().await.unwrap();
    let items = find(&tables, Some("staged_charn"), "items").clone();
    // The truncation trap must be armed for this test to mean anything:
    // introspection reports the bare, modifier-less name.
    assert_eq!(
        items
            .columns
            .iter()
            .find(|c| c.name == "tag")
            .unwrap()
            .type_name,
        "character"
    );
    let identity = detect_row_identity(&items, Dialect::Postgres).unwrap();

    // SET on a character(3) column keeps the full value, and the
    // character(3) PRIMARY KEY addresses its row (guard passes).
    let key = vec![Value::Text("abc".into())];
    let changes = vec![
        update(key.clone(), "tag", Value::Text("xyz".into())),
        update(key, "label", Value::Text("two".into())),
    ];
    let counts = apply_staged(
        &pool,
        &pool.backend_access(&items),
        &items,
        &identity,
        &changes,
    )
    .await
    .unwrap();
    assert_eq!(counts.updated_rows, 1);

    let check = pool
        .query("SELECT label, tag FROM staged_charn.items WHERE code = 'abc'")
        .await
        .unwrap();
    assert_eq!(check.rows[0][0], Value::Text("two".into()));
    assert_eq!(
        check.rows[0][1],
        Value::Text("xyz".into()),
        "character(3) SET must not truncate to char(1)"
    );

    pool.close().await;
}

/// bit(n) columns cannot take a text parameter at all (text → bit has no
/// assignment cast, and `$1::bit` would mean bit(1)); with the cast
/// skipped the save fails LOUDLY and rolls back — never a silent wrong
/// value. Documented limitation until a dedicated bit editor exists.
#[tokio::test]
async fn postgres_bit_n_edit_fails_loudly_and_rolls_back() {
    let Some(url) = test_url() else { return };
    let pool = DbPool::open_postgres(&url).await.unwrap();
    for sql in [
        "DROP SCHEMA IF EXISTS staged_bitn CASCADE",
        "CREATE SCHEMA staged_bitn",
        "CREATE TABLE staged_bitn.items (
            id integer PRIMARY KEY,
            mask bit(3)
        )",
        "INSERT INTO staged_bitn.items VALUES (1, B'101')",
    ] {
        pool.query(sql).await.unwrap();
    }
    let tables = pool.introspect().await.unwrap();
    let items = find(&tables, Some("staged_bitn"), "items").clone();
    let identity = detect_row_identity(&items, Dialect::Postgres).unwrap();

    let changes = vec![update(
        vec![Value::Integer(1)],
        "mask",
        Value::Text("110".into()),
    )];
    let err = apply_staged(
        &pool,
        &pool.backend_access(&items),
        &items,
        &identity,
        &changes,
    )
    .await
    .unwrap_err();
    assert_eq!(err.change_index, Some(0), "the edit itself is named");

    let check = pool
        .query("SELECT mask::text FROM staged_bitn.items WHERE id = 1")
        .await
        .unwrap();
    assert_eq!(
        check.rows[0][0],
        Value::Text("101".into()),
        "failed bit edit must roll back, never store a wrong value"
    );

    pool.close().await;
}

/// INSERT values coerce through the same casts (FRE-25 will stage inserts
/// with text values from the same editors).
#[tokio::test]
async fn postgres_staged_text_insert_coerces_to_column_types() {
    let Some(url) = test_url() else { return };
    let pool = DbPool::open_postgres(&url).await.unwrap();
    for sql in [
        "DROP SCHEMA IF EXISTS staged_cast_insert CASCADE",
        "CREATE SCHEMA staged_cast_insert",
        "CREATE TABLE staged_cast_insert.typed (
            id integer PRIMARY KEY,
            flag boolean,
            doc jsonb
        )",
    ] {
        pool.query(sql).await.unwrap();
    }
    let tables = pool.introspect().await.unwrap();
    let typed = find(&tables, Some("staged_cast_insert"), "typed").clone();
    let identity = detect_row_identity(&typed, Dialect::Postgres).unwrap();

    let changes = vec![StagedChange::Insert {
        columns: vec!["id".into(), "flag".into(), "doc".into()],
        values: vec![
            Value::Text("7".into()),
            bool_value(Dialect::Postgres, true),
            Value::Text("[1, 2]".into()),
        ],
    }];
    let counts = apply_staged(
        &pool,
        &pool.backend_access(&typed),
        &typed,
        &identity,
        &changes,
    )
    .await
    .unwrap();
    assert_eq!(counts.inserted_rows, 1);

    let check = pool
        .query("SELECT id, flag, doc FROM staged_cast_insert.typed")
        .await
        .unwrap();
    assert_eq!(check.rows[0][0], Value::Integer(7));
    assert_eq!(check.rows[0][1], Value::Text("true".into()));
    assert_eq!(check.rows[0][2], Value::Text("[1,2]".into()));

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
    let counts = apply_staged(
        &pool,
        &pool.backend_access(&items),
        &items,
        &identity,
        &changes,
    )
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

/// FRE-25 sqlite insert flow, staged the way the grid does it (per-column
/// overrides on a `TableStage`): only overridden columns reach the INSERT,
/// the INTEGER PRIMARY KEY (rowid alias — exempt from the required set) and
/// the defaulted column get their values from the database, and a fully
/// default phantom row inserts via DEFAULT VALUES.
#[tokio::test]
async fn sqlite_staged_insert_leaves_defaults_to_the_database() {
    let fixture = FixtureDb::with_sql(
        "CREATE TABLE items (
            id INTEGER PRIMARY KEY,
            label TEXT NOT NULL,
            qty INTEGER NOT NULL DEFAULT 5,
            note TEXT
        );",
    )
    .await;
    let pool = fixture.open().await;
    let tables = pool.introspect().await.unwrap();
    let items = find(&tables, None, "items").clone();
    let identity = detect_row_identity(&items, Dialect::Sqlite).unwrap();

    // Required-column boundary: only `label` is NOT NULL without a default
    // that the database won't auto-assign (id is the rowid alias).
    let required = required_insert_columns(&items, Dialect::Sqlite);
    assert_eq!(required, HashSet::from(["label".to_string()]));

    let mut stage = TableStage::default();
    let insert = stage.add_insert();
    assert_eq!(stage.missing_required(&required), 1, "label unfilled");
    stage.set_insert_value(insert, "label", Value::Text("widget".into()));
    assert_eq!(
        stage.missing_required(&required),
        0,
        "label filled → saveable"
    );

    let counts = apply_staged(
        &pool,
        &pool.backend_access(&items),
        &items,
        &identity,
        &stage.changes(),
    )
    .await
    .unwrap();
    assert_eq!(counts.inserted_rows, 1);

    // The database assigned id (rowid alias) and the qty default.
    let check = pool
        .query("SELECT id, label, qty, note FROM items")
        .await
        .unwrap();
    assert_eq!(check.rows.len(), 1);
    assert_eq!(check.rows[0][0], Value::Integer(1), "db-assigned id");
    assert_eq!(check.rows[0][1], Value::Text("widget".into()));
    assert_eq!(check.rows[0][2], Value::Integer(5), "column default");
    assert_eq!(check.rows[0][3], Value::Null, "nullable column left NULL");

    // A second, fully defaultable table: an all-default phantom row goes
    // through INSERT … DEFAULT VALUES.
    pool.query("CREATE TABLE logs (id INTEGER PRIMARY KEY, note TEXT DEFAULT 'x')")
        .await
        .unwrap();
    let tables = pool.introspect().await.unwrap();
    let logs = find(&tables, None, "logs").clone();
    let logs_identity = detect_row_identity(&logs, Dialect::Sqlite).unwrap();
    assert!(required_insert_columns(&logs, Dialect::Sqlite).is_empty());
    let mut stage = TableStage::default();
    stage.add_insert();
    assert_eq!(
        stage.changes(),
        vec![StagedChange::Insert {
            columns: vec![],
            values: vec![],
        }]
    );
    apply_staged(
        &pool,
        &pool.backend_access(&logs),
        &logs,
        &logs_identity,
        &stage.changes(),
    )
    .await
    .unwrap();
    let check = pool.query("SELECT id, note FROM logs").await.unwrap();
    assert_eq!(check.rows[0][0], Value::Integer(1));
    assert_eq!(check.rows[0][1], Value::Text("x".into()));

    pool.close().await;
}

/// FRE-25 postgres insert flow: serial, identity, and stored generated
/// columns are all exempt from the required set (nextval default,
/// `Generated::ByDefault`/`Always`), stay out of the INSERT when not
/// overridden, and get database-assigned values — verified by re-query. The
/// NOT NULL stored generated column is the regression case: it must NOT be
/// flagged required (filling it would make the INSERT fail unconditionally).
#[tokio::test]
async fn postgres_staged_insert_gets_serial_and_identity_values() {
    use hubro::db::Generated;

    let Some(url) = test_url() else { return };
    let pool = DbPool::open_postgres(&url).await.unwrap();
    for sql in [
        "DROP SCHEMA IF EXISTS staged_insert_defaults CASCADE",
        "CREATE SCHEMA staged_insert_defaults",
        "CREATE TABLE staged_insert_defaults.items (
            id serial PRIMARY KEY,
            seq integer GENERATED ALWAYS AS IDENTITY,
            by_default integer GENERATED BY DEFAULT AS IDENTITY,
            label text NOT NULL,
            qty integer NOT NULL DEFAULT 7,
            doubled integer GENERATED ALWAYS AS (qty * 2) STORED NOT NULL,
            note text
        )",
    ] {
        pool.query(sql).await.unwrap();
    }
    let tables = pool.introspect().await.unwrap();
    let items = find(&tables, Some("staged_insert_defaults"), "items").clone();
    let identity = detect_row_identity(&items, Dialect::Postgres).unwrap();

    // Introspection surfaces each auto-assignment: serial via its real
    // nextval default, the others via `ColumnMeta::generated`.
    let by_name = |name: &str| items.columns.iter().find(|c| c.name == name).unwrap();
    assert!(by_name("id")
        .default
        .as_deref()
        .unwrap()
        .contains("nextval"));
    assert_eq!(by_name("id").generated, Generated::Never);
    assert_eq!(by_name("seq").generated, Generated::Always);
    assert!(by_name("seq").default.is_none());
    assert_eq!(by_name("by_default").generated, Generated::ByDefault);
    // The NOT NULL stored generated column: database-assigned, read-only.
    assert_eq!(by_name("doubled").generated, Generated::Always);
    assert!(!by_name("doubled").nullable);

    // Required-column boundary: only `label` must be filled — critically,
    // the NOT NULL `doubled` generated column is NOT required.
    let required = required_insert_columns(&items, Dialect::Postgres);
    assert_eq!(required, HashSet::from(["label".to_string()]));

    let mut stage = TableStage::default();
    let insert = stage.add_insert();
    assert_eq!(stage.missing_required(&required), 1);
    stage.set_insert_value(insert, "label", Value::Text("gadget".into()));
    assert_eq!(stage.missing_required(&required), 0);
    // The generated change carries ONLY the overridden column.
    assert_eq!(
        stage.changes(),
        vec![StagedChange::Insert {
            columns: vec!["label".into()],
            values: vec![Value::Text("gadget".into())],
        }]
    );

    let counts = apply_staged(
        &pool,
        &pool.backend_access(&items),
        &items,
        &identity,
        &stage.changes(),
    )
    .await
    .unwrap();
    assert_eq!(counts.inserted_rows, 1);

    let check = pool
        .query(
            "SELECT id, seq, by_default, label, qty, doubled, note \
             FROM staged_insert_defaults.items",
        )
        .await
        .unwrap();
    assert_eq!(check.rows.len(), 1);
    assert_eq!(check.rows[0][0], Value::Integer(1), "serial assigned");
    assert_eq!(check.rows[0][1], Value::Integer(1), "identity assigned");
    assert_eq!(check.rows[0][2], Value::Integer(1), "by-default assigned");
    assert_eq!(check.rows[0][3], Value::Text("gadget".into()));
    assert_eq!(check.rows[0][4], Value::Integer(7), "column default");
    assert_eq!(check.rows[0][5], Value::Integer(14), "stored generated");
    assert_eq!(check.rows[0][6], Value::Null);

    pool.close().await;
}

/// FRE-25 multi-delete: two selected rows staged for deletion plus an edit,
/// all in ONE transaction; the exact delete count the confirmation showed is
/// what lands.
#[tokio::test]
async fn sqlite_staged_multi_delete_with_edit_applies_exact_counts() {
    let fixture = FixtureDb::with_sql(
        "CREATE TABLE contacts (id INTEGER PRIMARY KEY, name TEXT NOT NULL);
         INSERT INTO contacts VALUES (1, 'ana'), (2, 'bo'), (3, 'cy'), (4, 'dee');",
    )
    .await;
    let pool = fixture.open().await;
    let tables = pool.introspect().await.unwrap();
    let contacts = find(&tables, None, "contacts").clone();
    let identity = detect_row_identity(&contacts, Dialect::Sqlite).unwrap();

    // Stage exactly as the grid does: selection → mark_delete per row, plus
    // an edit of a surviving row.
    let mut stage = TableStage::default();
    stage.mark_delete(locator(vec![Value::Integer(2)]));
    stage.mark_delete(locator(vec![Value::Integer(3)]));
    stage.set_cell_edit(
        locator(vec![Value::Integer(1)]),
        "name",
        Value::Text("Sole Survivor".into()),
    );
    // This is the count the save-time confirmation shows.
    assert_eq!(stage.delete_count(), 2);
    assert_eq!(stage.pending_count(), 3);

    let counts = apply_staged(
        &pool,
        &pool.backend_access(&contacts),
        &contacts,
        &identity,
        &stage.changes(),
    )
    .await
    .unwrap();
    assert_eq!(counts.deleted_rows, 2, "exactly the confirmed count");
    assert_eq!(counts.updated_rows, 1);
    assert_eq!(counts.inserted_rows, 0);

    let check = pool
        .query("SELECT id, name FROM contacts ORDER BY id")
        .await
        .unwrap();
    assert_eq!(check.rows.len(), 2);
    assert_eq!(check.rows[0][0], Value::Integer(1));
    assert_eq!(check.rows[0][1], Value::Text("Sole Survivor".into()));
    assert_eq!(check.rows[1][0], Value::Integer(4), "row 4 untouched");

    pool.close().await;
}

/// FRE-111: the marking is enforced in `db/`, not only in the UI. These drive
/// the real write paths against a real database with a `ReadOnly`-resolved
/// access, so a UI gate that someone later removes cannot let a write land.
#[tokio::test]
async fn a_read_only_marking_refuses_a_staged_write_and_leaves_the_row_untouched() {
    let fixture = FixtureDb::full().await;
    let pool = fixture.open().await;
    let tables = pool.introspect().await.unwrap();
    let albums = find(&tables, None, "albums").clone();
    let identity = detect_row_identity(&albums, Dialect::Sqlite).unwrap();

    let before = pool
        .query("SELECT title FROM albums WHERE artist_id = 1 AND seq = 1")
        .await
        .unwrap();

    let access = TableAccess::resolve_protected(
        pool.backend_capabilities(),
        WriteProtection::ReadOnly,
        &albums,
        Dialect::Sqlite,
    );
    let err = apply_staged(
        &pool,
        &access,
        &albums,
        &identity,
        &[update(
            vec![Value::Integer(1), Value::Integer(1)],
            "title",
            Value::Text("should never land".into()),
        )],
    )
    .await
    .expect_err("a connection marked read-only must refuse staged writes");
    assert!(
        err.message.contains("marked this connection read-only"),
        "the refusal must name the marking, not the engine: {}",
        err.message
    );

    let after = pool
        .query("SELECT title FROM albums WHERE artist_id = 1 AND seq = 1")
        .await
        .unwrap();
    assert_eq!(before.rows, after.rows, "nothing may have been written");
    pool.close().await;
}

#[tokio::test]
async fn a_read_only_marking_refuses_a_delete_from_the_sql_editor() {
    let fixture = FixtureDb::full().await;
    let pool = fixture.open().await;
    let caps = WriteProtection::ReadOnly.apply(pool.backend_capabilities());

    let statements = vec!["DELETE FROM albums".to_string()];
    let err = run_script(&pool, caps, &statements, |_| {})
        .await
        .expect_err("a script write must be refused on a marked connection");
    assert!(!err.rolled_back, "nothing ran, so nothing was rolled back");

    let count = pool.query("SELECT COUNT(*) FROM albums").await.unwrap();
    assert_ne!(
        count.rows[0][0],
        Value::Integer(0),
        "the table must still have its rows"
    );
    pool.close().await;
}

#[tokio::test]
async fn confirm_marking_does_not_itself_block_a_write() {
    // Confirm interposes a prompt in the UI; at the db/ layer it must behave
    // exactly like an unmarked connection, or the prompt would lead nowhere.
    let fixture = FixtureDb::full().await;
    let pool = fixture.open().await;
    let tables = pool.introspect().await.unwrap();
    let albums = find(&tables, None, "albums").clone();
    let identity = detect_row_identity(&albums, Dialect::Sqlite).unwrap();

    let access = TableAccess::resolve_protected(
        pool.backend_capabilities(),
        WriteProtection::Confirm,
        &albums,
        Dialect::Sqlite,
    );
    let counts = apply_staged(
        &pool,
        &access,
        &albums,
        &identity,
        &[update(
            vec![Value::Integer(1), Value::Integer(1)],
            "title",
            Value::Text("confirmed".into()),
        )],
    )
    .await
    .expect("Confirm must not block the write once it reaches db/");
    assert_eq!(counts.updated_rows, 1);
    pool.close().await;
}
