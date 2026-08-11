//! End-to-end tests for row-identity detection and guarded writes
//! (FRE-26): detection runs on real introspection output, and the write path
//! commits only when the affected-row count matches.
//!
//! The write cases drive `apply_staged` — the real production path, which
//! builds its statements from the detected [`RowIdentity`] and runs them
//! through `execute_all_checked` — rather than a test-only SQL builder, so
//! what is verified here is what the app actually sends.
//!
//! Postgres cases need a running server (Docker only, per CLAUDE.md) and are
//! skipped unless `HUBRO_PG_TEST_URL` is set — see tests/db_postgres.rs
//! for the docker run recipe.

mod common;

use common::FixtureDb;
use hubro::db::{
    apply_staged, detect_row_identity, DbError, DbPool, Dialect, RowIdentity, RowLocator,
    StagedChange, TableKind, TableMeta, Value,
};

fn locator(values: Vec<Value>) -> RowLocator {
    RowLocator {
        identity_values: values,
    }
}

async fn test_url() -> Option<String> {
    common::pg_test_url().await
}

fn find<'a>(tables: &'a [TableMeta], schema: Option<&str>, name: &str) -> &'a TableMeta {
    tables
        .iter()
        .find(|t| t.schema.as_deref() == schema && t.name == name)
        .unwrap_or_else(|| panic!("no table {schema:?}.{name}"))
}

#[tokio::test]
async fn sqlite_detection_on_real_introspection_output() {
    let fixture = FixtureDb::full().await;
    let pool = fixture.open().await;
    let tables = pool.introspect().await.unwrap();

    // INTEGER PRIMARY KEY (which *is* the rowid) resolves to the PK.
    assert_eq!(
        detect_row_identity(find(&tables, None, "artists"), Dialect::Sqlite),
        Some(RowIdentity::PrimaryKey {
            columns: vec!["id".into()]
        })
    );
    // Composite PK, in key order.
    assert_eq!(
        detect_row_identity(find(&tables, None, "albums"), Dialect::Sqlite),
        Some(RowIdentity::PrimaryKey {
            columns: vec!["artist_id".into(), "seq".into()]
        })
    );
    // WITHOUT ROWID table: has a PK by construction, never hits the
    // rowid fallback.
    assert_eq!(
        detect_row_identity(find(&tables, None, "settings"), Dialect::Sqlite),
        Some(RowIdentity::PrimaryKey {
            columns: vec!["key".into(), "scope".into()]
        })
    );
    // Views are read-only.
    let view = find(&tables, None, "artist_overview");
    assert_eq!(view.kind, TableKind::View);
    assert_eq!(detect_row_identity(view, Dialect::Sqlite), None);

    pool.close().await;
}

#[tokio::test]
async fn sqlite_fallbacks_on_real_introspection_output() {
    let fixture = FixtureDb::with_sql(
        "CREATE TABLE by_unique (email TEXT NOT NULL, bio TEXT);
         CREATE UNIQUE INDEX uniq_email ON by_unique(email);
         CREATE TABLE by_rowid (data TEXT);
         CREATE TABLE nullable_unique (email TEXT, note TEXT);
         CREATE UNIQUE INDEX uniq_nullable_email ON nullable_unique(email);
         CREATE TABLE expr_unique (email TEXT NOT NULL);
         CREATE UNIQUE INDEX uniq_lower_email ON expr_unique(lower(email));
         CREATE TABLE partial_unique (email TEXT NOT NULL, active INTEGER);
         CREATE UNIQUE INDEX uniq_active_email ON partial_unique(email)
             WHERE active = 1;
         INSERT INTO partial_unique (email, active) VALUES
             ('dup@x', 0), ('dup@x', 0);",
    )
    .await;
    let pool = fixture.open().await;
    let tables = pool.introspect().await.unwrap();

    // No PK, but a NOT NULL unique index: used, and attributed by name.
    assert_eq!(
        detect_row_identity(find(&tables, None, "by_unique"), Dialect::Sqlite),
        Some(RowIdentity::UniqueIndex {
            name: "uniq_email".into(),
            columns: vec!["email".into()]
        })
    );
    // No key at all: a plain SQLite table still has its implicit rowid.
    assert_eq!(
        detect_row_identity(find(&tables, None, "by_rowid"), Dialect::Sqlite),
        Some(RowIdentity::Rowid {
            column: "rowid".into()
        })
    );
    // Nullable / expression / partial unique indexes are rejected — rowid
    // it is. The partial-index fixture even holds duplicate emails outside
    // the predicate's partition, which "email = ?" could never tell apart.
    for name in ["nullable_unique", "expr_unique", "partial_unique"] {
        assert_eq!(
            detect_row_identity(find(&tables, None, name), Dialect::Sqlite),
            Some(RowIdentity::Rowid {
                column: "rowid".into()
            }),
            "table {name}"
        );
    }
    // Introspection carries the partial flag itself (the rejection above
    // depends on it).
    let partial = find(&tables, None, "partial_unique");
    let idx = partial
        .indexes
        .iter()
        .find(|i| i.name == "uniq_active_email")
        .unwrap();
    assert!(idx.unique && idx.partial);

    pool.close().await;
}

#[tokio::test]
async fn sqlite_staged_writes_commit_through_the_full_key() {
    let fixture = FixtureDb::full().await;
    let pool = fixture.open().await;
    let tables = pool.introspect().await.unwrap();
    let albums = find(&tables, None, "albums").clone();
    let identity = detect_row_identity(&albums, Dialect::Sqlite).unwrap();
    let access = pool.backend_access(&albums);

    // UPDATE targeting one row through the full composite key.
    let counts = apply_staged(
        &pool,
        &access,
        &albums,
        &identity,
        &[StagedChange::Update {
            locator: locator(vec![Value::Integer(1), Value::Integer(2)]),
            column: "title".into(),
            value: Value::Text("Renamed".into()),
        }],
    )
    .await
    .unwrap();
    assert_eq!(counts.updated_rows, 1);
    let check = pool
        .query("SELECT title FROM albums WHERE artist_id = 1 AND seq = 2")
        .await
        .unwrap();
    assert_eq!(check.rows[0][0], Value::Text("Renamed".into()));
    // The sibling row of the same artist was not touched.
    let sibling = pool
        .query("SELECT title FROM albums WHERE artist_id = 1 AND seq = 1")
        .await
        .unwrap();
    assert_eq!(sibling.rows[0][0], Value::Text("First".into()));

    // DELETE through the full key commits exactly one row.
    let counts = apply_staged(
        &pool,
        &access,
        &albums,
        &identity,
        &[StagedChange::Delete {
            locator: locator(vec![Value::Integer(2), Value::Integer(1)]),
        }],
    )
    .await
    .unwrap();
    assert_eq!(counts.deleted_rows, 1);
    let count = pool.query("SELECT COUNT(*) FROM albums").await.unwrap();
    assert_eq!(count.rows[0][0], Value::Integer(2));

    pool.close().await;
}

#[tokio::test]
async fn sqlite_over_matching_write_rolls_back() {
    let fixture = FixtureDb::full().await;
    let pool = fixture.open().await;

    // Deliberately over-matching: two albums belong to artist 1.
    let err = pool
        .execute_checked(
            "UPDATE \"albums\" SET \"title\" = ? WHERE \"artist_id\" = ?",
            &[Value::Text("clobbered".into()), Value::Integer(1)],
            1,
        )
        .await
        .unwrap_err();
    match &err {
        DbError::RowCountMismatch(msg) => {
            assert!(msg.contains("affected 2 rows"), "message: {msg}");
            assert!(msg.contains("expected 1"), "message: {msg}");
            assert!(msg.contains("rolled back"), "message: {msg}");
        }
        other => panic!("expected RowCountMismatch, got {other:?}"),
    }
    // The transaction rolled back: no row was clobbered.
    let check = pool
        .query("SELECT COUNT(*) FROM albums WHERE title = 'clobbered'")
        .await
        .unwrap();
    assert_eq!(check.rows[0][0], Value::Integer(0));
    let titles = pool
        .query("SELECT title FROM albums WHERE artist_id = 1 ORDER BY seq")
        .await
        .unwrap();
    assert_eq!(titles.rows[0][0], Value::Text("First".into()));
    assert_eq!(titles.rows[1][0], Value::Text("Second".into()));

    pool.close().await;
}

#[tokio::test]
async fn sqlite_rowid_identity_edits_a_keyless_table() {
    let fixture = FixtureDb::with_sql(
        "CREATE TABLE notes (body TEXT);
         INSERT INTO notes (body) VALUES ('same'), ('same'), ('other');",
    )
    .await;
    let pool = fixture.open().await;
    let tables = pool.introspect().await.unwrap();
    let notes = find(&tables, None, "notes").clone();
    let identity = detect_row_identity(&notes, Dialect::Sqlite).unwrap();
    assert_eq!(
        identity,
        RowIdentity::Rowid {
            column: "rowid".into()
        }
    );

    // Two rows have identical column values; the rowid still addresses
    // exactly one of them.
    let counts = apply_staged(
        &pool,
        &pool.backend_access(&notes),
        &notes,
        &identity,
        &[StagedChange::Update {
            locator: locator(vec![Value::Integer(1)]),
            column: "body".into(),
            value: Value::Text("edited".into()),
        }],
    )
    .await
    .unwrap();
    assert_eq!(counts.updated_rows, 1);
    let check = pool
        .query("SELECT rowid, body FROM notes ORDER BY rowid")
        .await
        .unwrap();
    assert_eq!(check.rows[0][1], Value::Text("edited".into()));
    assert_eq!(check.rows[1][1], Value::Text("same".into()));

    pool.close().await;
}

#[tokio::test]
async fn postgres_detection_on_real_introspection_output() {
    let Some(url) = test_url().await else { return };
    let pool = DbPool::open_postgres(&url).await.unwrap();
    for sql in [
        "DROP SCHEMA IF EXISTS rowkey CASCADE",
        "CREATE SCHEMA rowkey",
        "CREATE TABLE rowkey.with_pk (id serial PRIMARY KEY, name text)",
        "CREATE TABLE rowkey.composite_pk (
            region text NOT NULL,
            slot integer NOT NULL,
            label text,
            PRIMARY KEY (region, slot)
        )",
        "CREATE TABLE rowkey.by_unique (email text NOT NULL, bio text)",
        "CREATE UNIQUE INDEX rowkey_uniq_email ON rowkey.by_unique (email)",
        "CREATE TABLE rowkey.nullable_unique (email text)",
        "CREATE UNIQUE INDEX rowkey_uniq_nullable ON rowkey.nullable_unique (email)",
        "CREATE TABLE rowkey.expr_unique (email text NOT NULL)",
        "CREATE UNIQUE INDEX rowkey_uniq_expr ON rowkey.expr_unique (lower(email))",
        "CREATE TABLE rowkey.partial_unique (email text NOT NULL, active boolean NOT NULL)",
        "CREATE UNIQUE INDEX rowkey_uniq_partial ON rowkey.partial_unique (email) WHERE active",
        "INSERT INTO rowkey.partial_unique VALUES ('dup@x', false), ('dup@x', false)",
        "CREATE TABLE rowkey.keyless (data text)",
        "CREATE VIEW rowkey.pk_view AS SELECT id FROM rowkey.with_pk",
    ] {
        pool.query(sql).await.unwrap();
    }

    let tables = pool.introspect().await.unwrap();
    let schema = Some("rowkey");
    assert_eq!(
        detect_row_identity(find(&tables, schema, "with_pk"), Dialect::Postgres),
        Some(RowIdentity::PrimaryKey {
            columns: vec!["id".into()]
        })
    );
    assert_eq!(
        detect_row_identity(find(&tables, schema, "composite_pk"), Dialect::Postgres),
        Some(RowIdentity::PrimaryKey {
            columns: vec!["region".into(), "slot".into()]
        })
    );
    assert_eq!(
        detect_row_identity(find(&tables, schema, "by_unique"), Dialect::Postgres),
        Some(RowIdentity::UniqueIndex {
            name: "rowkey_uniq_email".into(),
            columns: vec!["email".into()]
        })
    );
    // Nullable unique, expression-only unique, partial unique, and keyless
    // tables are all read-only on Postgres — no rowid to fall back to.
    // `detect_row_identity == None` on a `TableKind::Table` is exactly the
    // condition the grid's read-only notice keys on.
    for name in [
        "nullable_unique",
        "expr_unique",
        "partial_unique",
        "keyless",
    ] {
        let table = find(&tables, schema, name);
        assert_eq!(table.kind, TableKind::Table);
        assert_eq!(
            detect_row_identity(table, Dialect::Postgres),
            None,
            "table {name}"
        );
    }
    // Introspection carries the partial flag itself (the rejection above
    // depends on it); the fixture even holds duplicate emails outside the
    // predicate's partition.
    let partial = find(&tables, schema, "partial_unique");
    let idx = partial
        .indexes
        .iter()
        .find(|i| i.name == "rowkey_uniq_partial")
        .unwrap();
    assert!(idx.unique && idx.partial);
    let view = find(&tables, schema, "pk_view");
    assert_eq!(view.kind, TableKind::View);
    assert_eq!(detect_row_identity(view, Dialect::Postgres), None);

    pool.close().await;
}

#[tokio::test]
async fn postgres_invalid_index_is_not_introspected() {
    let Some(url) = test_url().await else { return };
    let pool = DbPool::open_postgres(&url).await.unwrap();
    for sql in [
        "DROP SCHEMA IF EXISTS rowkey_invalid CASCADE",
        "CREATE SCHEMA rowkey_invalid",
        "CREATE TABLE rowkey_invalid.dups (email text NOT NULL)",
        "INSERT INTO rowkey_invalid.dups VALUES ('dup@x'), ('dup@x')",
    ] {
        pool.query(sql).await.unwrap();
    }
    // CREATE UNIQUE INDEX CONCURRENTLY over duplicate data fails partway
    // and deterministically leaves an INVALID index entry behind (Postgres
    // documents that the failed index remains and must be dropped
    // manually). An invalid index guarantees nothing — introspection must
    // not surface it, or row identity could trust a broken index.
    pool.query(
        "CREATE UNIQUE INDEX CONCURRENTLY rowkey_invalid_uniq ON rowkey_invalid.dups (email)",
    )
    .await
    .expect_err("unique index over duplicates must fail");
    let exists = pool
        .query(
            "SELECT ix.indisvalid::text FROM pg_index ix \
             JOIN pg_class i ON i.oid = ix.indexrelid \
             WHERE i.relname = 'rowkey_invalid_uniq'",
        )
        .await
        .unwrap();
    assert_eq!(
        exists.rows.first().map(|r| r[0].clone()),
        Some(Value::Text("false".into())),
        "precondition: the failed CONCURRENTLY build left an invalid index"
    );

    let tables = pool.introspect().await.unwrap();
    let dups = find(&tables, Some("rowkey_invalid"), "dups");
    assert!(
        dups.indexes.iter().all(|i| i.name != "rowkey_invalid_uniq"),
        "invalid index must be dropped from metadata: {:?}",
        dups.indexes
    );
    assert_eq!(detect_row_identity(dups, Dialect::Postgres), None);

    pool.close().await;
}

#[tokio::test]
async fn postgres_staged_writes_guard_and_commit() {
    let Some(url) = test_url().await else { return };
    let pool = DbPool::open_postgres(&url).await.unwrap();
    for sql in [
        "DROP SCHEMA IF EXISTS rowkey_exec CASCADE",
        "CREATE SCHEMA rowkey_exec",
        "CREATE TABLE rowkey_exec.albums (
            artist_id integer NOT NULL,
            seq integer NOT NULL,
            title text NOT NULL,
            PRIMARY KEY (artist_id, seq)
        )",
        "INSERT INTO rowkey_exec.albums VALUES
            (1, 1, 'First'), (1, 2, 'Second'), (2, 1, 'Solo')",
    ] {
        pool.query(sql).await.unwrap();
    }
    let tables = pool.introspect().await.unwrap();
    let albums = find(&tables, Some("rowkey_exec"), "albums").clone();
    let identity = detect_row_identity(&albums, Dialect::Postgres).unwrap();

    // Successful single-row UPDATE via the full key.
    let access = pool.backend_access(&albums);
    let counts = apply_staged(
        &pool,
        &access,
        &albums,
        &identity,
        &[StagedChange::Update {
            locator: locator(vec![Value::Integer(1), Value::Integer(2)]),
            column: "title".into(),
            value: Value::Text("Renamed".into()),
        }],
    )
    .await
    .unwrap();
    assert_eq!(counts.updated_rows, 1);
    let check = pool
        .query("SELECT title FROM rowkey_exec.albums WHERE artist_id = 1 AND seq = 2")
        .await
        .unwrap();
    assert_eq!(check.rows[0][0], Value::Text("Renamed".into()));

    // Over-matching statement (expected 1, matches 2) rolls back.
    let err = pool
        .execute_checked(
            "UPDATE rowkey_exec.albums SET title = $1 WHERE artist_id = $2",
            &[Value::Text("clobbered".into()), Value::Integer(1)],
            1,
        )
        .await
        .unwrap_err();
    match &err {
        DbError::RowCountMismatch(msg) => {
            assert!(msg.contains("affected 2 rows"), "message: {msg}");
        }
        other => panic!("expected RowCountMismatch, got {other:?}"),
    }
    let check = pool
        .query("SELECT COUNT(*) FROM rowkey_exec.albums WHERE title = 'clobbered'")
        .await
        .unwrap();
    assert_eq!(check.rows[0][0], Value::Integer(0));

    // Expected == actual DELETE commits.
    let counts = apply_staged(
        &pool,
        &access,
        &albums,
        &identity,
        &[StagedChange::Delete {
            locator: locator(vec![Value::Integer(2), Value::Integer(1)]),
        }],
    )
    .await
    .unwrap();
    assert_eq!(counts.deleted_rows, 1);
    let count = pool
        .query("SELECT COUNT(*) FROM rowkey_exec.albums")
        .await
        .unwrap();
    assert_eq!(count.rows[0][0], Value::Integer(2));

    pool.close().await;
}
