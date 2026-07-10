//! Integration tests for the SQLite database layer: open, introspect, query.

use std::path::PathBuf;

use dataview::db::{DbError, DbPool, TableKind, Value};
use sqlx::sqlite::SqliteConnectOptions;
use sqlx::SqlitePool;

/// Creates a database file with a schema covering tables, views, indexes,
/// composite PKs and FKs, and all storage classes, then closes it.
async fn create_fixture(dir: &tempfile::TempDir) -> PathBuf {
    let path = dir.path().join("fixture.db");
    let options = SqliteConnectOptions::new()
        .filename(&path)
        .create_if_missing(true);
    let pool = SqlitePool::connect_with(options).await.unwrap();
    sqlx::query(
        r#"
        CREATE TABLE artists (
            id INTEGER PRIMARY KEY,
            name TEXT NOT NULL,
            rating REAL,
            cover BLOB,
            notes TEXT DEFAULT 'none'
        );
        CREATE TABLE albums (
            artist_id INTEGER NOT NULL REFERENCES artists(id),
            seq INTEGER NOT NULL,
            title TEXT NOT NULL,
            PRIMARY KEY (artist_id, seq)
        );
        CREATE UNIQUE INDEX idx_artists_name ON artists(name);
        CREATE INDEX idx_albums_title ON albums(title);
        CREATE VIEW artist_names AS SELECT name FROM artists;
        INSERT INTO artists (id, name, rating, cover, notes)
            VALUES (1, 'Ana', 4.5, x'0102', NULL);
        INSERT INTO artists (id, name, rating, cover, notes)
            VALUES (2, 'Bo', NULL, NULL, 'good');
        INSERT INTO albums VALUES (1, 1, 'First');
        "#,
    )
    .execute(&pool)
    .await
    .unwrap();
    pool.close().await;
    path
}

#[tokio::test]
async fn open_missing_file_is_a_connect_error() {
    let err = DbPool::open_sqlite(std::path::Path::new("/nonexistent/nope.db"))
        .await
        .err()
        .expect("opening a missing file must fail");
    assert!(matches!(err, DbError::Connect(_)));
}

#[tokio::test]
async fn open_non_database_file_is_a_connect_error() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("garbage.db");
    std::fs::write(&path, b"this is not a sqlite database, not even close!").unwrap();
    let err = DbPool::open_sqlite(&path)
        .await
        .err()
        .expect("opening a non-database file must fail");
    assert!(matches!(err, DbError::Connect(_)));
}

#[tokio::test]
async fn introspection_captures_columns_keys_indexes_and_fks() {
    let dir = tempfile::tempdir().unwrap();
    let path = create_fixture(&dir).await;
    let pool = DbPool::open_sqlite(&path).await.unwrap();

    let tables = pool.introspect().await.unwrap();
    let names: Vec<&str> = tables.iter().map(|t| t.name.as_str()).collect();
    assert_eq!(names, ["albums", "artist_names", "artists"]);

    let artists = tables.iter().find(|t| t.name == "artists").unwrap();
    assert_eq!(artists.kind, TableKind::Table);
    let id = artists.columns.iter().find(|c| c.name == "id").unwrap();
    assert_eq!(id.primary_key_position, Some(1));
    assert_eq!(id.type_name, "INTEGER");
    let name = artists.columns.iter().find(|c| c.name == "name").unwrap();
    assert!(!name.nullable);
    let notes = artists.columns.iter().find(|c| c.name == "notes").unwrap();
    assert_eq!(notes.default.as_deref(), Some("'none'"));
    assert!(artists
        .indexes
        .iter()
        .any(|i| i.name == "idx_artists_name" && i.unique && i.columns == ["name"]));

    let albums = tables.iter().find(|t| t.name == "albums").unwrap();
    let pk: Vec<&str> = albums
        .primary_key()
        .iter()
        .map(|c| c.name.as_str())
        .collect();
    assert_eq!(pk, ["artist_id", "seq"]);
    assert!(albums
        .indexes
        .iter()
        .any(|i| i.name == "idx_albums_title" && !i.unique));
    assert_eq!(albums.foreign_keys.len(), 1);
    let fk = &albums.foreign_keys[0];
    assert_eq!(fk.columns, ["artist_id"]);
    assert_eq!(fk.referenced_table, "artists");
    assert_eq!(fk.referenced_columns, [Some("id".to_string())]);

    let view = tables.iter().find(|t| t.name == "artist_names").unwrap();
    assert_eq!(view.kind, TableKind::View);
    assert_eq!(view.columns.len(), 1);

    pool.close().await;
}

#[tokio::test]
async fn query_decodes_all_storage_classes() {
    let dir = tempfile::tempdir().unwrap();
    let path = create_fixture(&dir).await;
    let pool = DbPool::open_sqlite(&path).await.unwrap();

    let result = pool
        .query("SELECT id, name, rating, cover, notes FROM artists ORDER BY id")
        .await
        .unwrap();
    let column_names: Vec<&str> = result.columns.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(column_names, ["id", "name", "rating", "cover", "notes"]);
    assert_eq!(result.rows.len(), 2);
    assert_eq!(
        result.rows[0],
        vec![
            Value::Integer(1),
            Value::Text("Ana".into()),
            Value::Real(4.5),
            Value::Blob(vec![1, 2]),
            Value::Null,
        ]
    );
    assert_eq!(result.rows[1][2], Value::Null);
    assert_eq!(result.rows[1][4], Value::Text("good".into()));

    let empty = pool.query("SELECT * FROM albums WHERE 0").await.unwrap();
    assert!(empty.rows.is_empty());

    let err = pool.query("SELECT * FROM missing_table").await.unwrap_err();
    assert!(matches!(err, DbError::Query(_)));

    pool.close().await;
}

#[tokio::test]
async fn weird_identifiers_survive_introspection() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("weird.db");
    let options = SqliteConnectOptions::new()
        .filename(&path)
        .create_if_missing(true);
    let setup = SqlitePool::connect_with(options).await.unwrap();
    sqlx::query(r#"CREATE TABLE "we""ird table" ("col name" TEXT, "übercol" INTEGER PRIMARY KEY)"#)
        .execute(&setup)
        .await
        .unwrap();
    setup.close().await;

    let pool = DbPool::open_sqlite(&path).await.unwrap();
    let tables = pool.introspect().await.unwrap();
    let table = tables.iter().find(|t| t.name == "we\"ird table").unwrap();
    let names: Vec<&str> = table.columns.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, ["col name", "übercol"]);
    pool.close().await;
}
