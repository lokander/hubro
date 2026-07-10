//! Integration tests for the SQLite database layer: open, introspect, query.

mod common;

use common::FixtureDb;
use dataview::db::{DbError, DbPool, TableKind, TableMeta, Value};

fn table<'a>(tables: &'a [TableMeta], name: &str) -> &'a TableMeta {
    tables
        .iter()
        .find(|t| t.name == name)
        .unwrap_or_else(|| panic!("table {name:?} missing from introspection"))
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
async fn introspection_lists_all_tables_and_views_sorted_by_name() {
    let fixture = FixtureDb::full().await;
    let pool = fixture.open().await;

    let tables = pool.introspect().await.unwrap();
    let names: Vec<&str> = tables.iter().map(|t| t.name.as_str()).collect();
    assert_eq!(
        names,
        [
            "albums",
            "artist_overview",
            "artists",
            "order",
            "settings",
            "tracks",
            "we\"ird table",
        ]
    );
    assert_eq!(table(&tables, "artist_overview").kind, TableKind::View);
    for name in ["albums", "artists", "order", "settings", "tracks"] {
        assert_eq!(table(&tables, name).kind, TableKind::Table);
    }

    pool.close().await;
}

#[tokio::test]
async fn introspection_captures_columns_defaults_and_composite_pk() {
    let fixture = FixtureDb::full().await;
    let pool = fixture.open().await;
    let tables = pool.introspect().await.unwrap();

    let artists = table(&tables, "artists");
    let id = artists.columns.iter().find(|c| c.name == "id").unwrap();
    assert_eq!(id.primary_key_position, Some(1));
    assert_eq!(id.type_name, "INTEGER");
    let name = artists.columns.iter().find(|c| c.name == "name").unwrap();
    assert!(!name.nullable);
    assert_eq!(name.primary_key_position, None);
    let rating = artists.columns.iter().find(|c| c.name == "rating").unwrap();
    assert!(rating.nullable);
    let notes = artists.columns.iter().find(|c| c.name == "notes").unwrap();
    assert_eq!(notes.default.as_deref(), Some("'none'"));

    let albums = table(&tables, "albums");
    let pk: Vec<&str> = albums
        .primary_key()
        .iter()
        .map(|c| c.name.as_str())
        .collect();
    assert_eq!(pk, ["artist_id", "seq"]);

    let view = table(&tables, "artist_overview");
    let view_columns: Vec<&str> = view.columns.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(view_columns, ["id", "name", "album_count"]);

    pool.close().await;
}

#[tokio::test]
async fn without_rowid_table_reports_pk_positions() {
    let fixture = FixtureDb::full().await;
    let pool = fixture.open().await;
    let tables = pool.introspect().await.unwrap();

    let settings = table(&tables, "settings");
    assert_eq!(settings.kind, TableKind::Table);
    let key = settings.columns.iter().find(|c| c.name == "key").unwrap();
    assert_eq!(key.primary_key_position, Some(1));
    let scope = settings.columns.iter().find(|c| c.name == "scope").unwrap();
    assert_eq!(scope.primary_key_position, Some(2));
    let pk: Vec<&str> = settings
        .primary_key()
        .iter()
        .map(|c| c.name.as_str())
        .collect();
    assert_eq!(pk, ["key", "scope"]);

    pool.close().await;
}

#[tokio::test]
async fn indexes_report_uniqueness_and_columns() {
    let fixture = FixtureDb::full().await;
    let pool = fixture.open().await;
    let tables = pool.introspect().await.unwrap();

    let artists = table(&tables, "artists");
    assert!(artists
        .indexes
        .iter()
        .any(|i| i.name == "idx_artists_name" && i.unique && i.columns == ["name"]));

    let albums = table(&tables, "albums");
    assert!(albums
        .indexes
        .iter()
        .any(|i| i.name == "idx_albums_title" && !i.unique && i.columns == ["title"]));

    pool.close().await;
}

#[tokio::test]
async fn expression_index_column_falls_back_to_expr_marker() {
    let fixture = FixtureDb::full().await;
    let pool = fixture.open().await;
    let tables = pool.introspect().await.unwrap();

    let tracks = table(&tables, "tracks");
    let expr_index = tracks
        .indexes
        .iter()
        .find(|i| i.name == "idx_tracks_title_lower")
        .expect("expression index must be listed");
    assert!(!expr_index.unique);
    assert_eq!(expr_index.columns, ["<expr>"]);

    pool.close().await;
}

#[tokio::test]
async fn multi_column_fk_is_grouped_into_one_fk_in_seq_order() {
    let fixture = FixtureDb::full().await;
    let pool = fixture.open().await;
    let tables = pool.introspect().await.unwrap();

    let tracks = table(&tables, "tracks");
    let fk = tracks
        .foreign_keys
        .iter()
        .find(|fk| fk.referenced_table == "albums")
        .expect("tracks must have an FK to albums");
    assert_eq!(fk.columns, ["album_artist_id", "album_seq"]);
    assert_eq!(
        fk.referenced_columns,
        [Some("artist_id".to_string()), Some("seq".to_string())]
    );

    pool.close().await;
}

#[tokio::test]
async fn fk_referencing_implicit_pk_has_no_referenced_column() {
    let fixture = FixtureDb::full().await;
    let pool = fixture.open().await;
    let tables = pool.introspect().await.unwrap();

    let tracks = table(&tables, "tracks");
    assert_eq!(tracks.foreign_keys.len(), 2);
    let fk = tracks
        .foreign_keys
        .iter()
        .find(|fk| fk.referenced_table == "artists")
        .expect("tracks must have an FK to artists");
    assert_eq!(fk.columns, ["composer_id"]);
    assert_eq!(fk.referenced_columns, [None]);

    pool.close().await;
}

#[tokio::test]
async fn single_column_fk_reports_both_sides() {
    let fixture = FixtureDb::full().await;
    let pool = fixture.open().await;
    let tables = pool.introspect().await.unwrap();

    let albums = table(&tables, "albums");
    assert_eq!(albums.foreign_keys.len(), 1);
    let fk = &albums.foreign_keys[0];
    assert_eq!(fk.columns, ["artist_id"]);
    assert_eq!(fk.referenced_table, "artists");
    assert_eq!(fk.referenced_columns, [Some("id".to_string())]);

    pool.close().await;
}

#[tokio::test]
async fn weird_identifiers_survive_introspection() {
    let fixture = FixtureDb::full().await;
    let pool = fixture.open().await;
    let tables = pool.introspect().await.unwrap();

    // Embedded double quote and space in the table name; space, unicode, and
    // an SQL keyword as column names.
    let weird = table(&tables, "we\"ird table");
    let names: Vec<&str> = weird.columns.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, ["col name", "übercol", "select"]);
    let select = weird.columns.iter().find(|c| c.name == "select").unwrap();
    assert_eq!(select.primary_key_position, Some(1));

    // An SQL keyword as table name, with a keyword column.
    let order = table(&tables, "order");
    let names: Vec<&str> = order.columns.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, ["id", "group"]);

    pool.close().await;
}

#[tokio::test]
async fn query_decodes_all_storage_classes() {
    let fixture = FixtureDb::full().await;
    let pool = fixture.open().await;

    let result = pool
        .query("SELECT id, name, rating, cover, notes FROM artists ORDER BY id")
        .await
        .unwrap();
    let column_names: Vec<&str> = result.columns.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(column_names, ["id", "name", "rating", "cover", "notes"]);
    assert_eq!(result.rows.len(), 3);
    assert_eq!(
        result.rows[0],
        vec![
            Value::Integer(1),
            Value::Text("Ana".into()),
            Value::Real(4.5),
            Value::Blob(vec![1, 2, 3]),
            Value::Null,
        ]
    );
    assert_eq!(result.rows[1][2], Value::Null);
    assert_eq!(result.rows[1][3], Value::Null);
    assert_eq!(result.rows[1][4], Value::Text("good".into()));
    // Empty blob stays a blob; it must not collapse to NULL or text.
    assert_eq!(result.rows[2][3], Value::Blob(vec![]));

    pool.close().await;
}

#[tokio::test]
async fn query_with_no_rows_yields_empty_result() {
    let fixture = FixtureDb::full().await;
    let pool = fixture.open().await;

    let empty = pool.query("SELECT * FROM albums WHERE 0").await.unwrap();
    assert!(empty.rows.is_empty());

    pool.close().await;
}

#[tokio::test]
async fn query_against_missing_table_is_a_query_error() {
    let fixture = FixtureDb::full().await;
    let pool = fixture.open().await;

    let err = pool.query("SELECT * FROM missing_table").await.unwrap_err();
    assert!(matches!(err, DbError::Query(_)));

    pool.close().await;
}

#[tokio::test]
async fn custom_schema_helper_builds_a_usable_database() {
    let fixture =
        FixtureDb::with_sql("CREATE TABLE t (x INTEGER); INSERT INTO t VALUES (1), (2), (3);")
            .await;
    let pool = fixture.open().await;

    let result = pool.query("SELECT sum(x) FROM t").await.unwrap();
    assert_eq!(result.rows[0][0], Value::Integer(6));

    pool.close().await;
}
