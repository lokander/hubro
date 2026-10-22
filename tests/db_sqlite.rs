//! Integration tests for the SQLite database layer: open, introspect, query.

mod common;

use common::FixtureDb;
use hubro::db::{
    apply_staged, explain_statement, Capabilities, DbError, DbPool, ExportFormat, Generated,
    PageRequest, PlanDisplay, Restriction, RowCount, RowIdentity, RowLocator, StagedChange,
    TableKind, TableMeta, Value, NUL_IN_SQL,
};

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
    // hubro's wording, naming the path — not the driver's "(code: 14) unable
    // to open database file", which says neither what went wrong nor where.
    assert!(
        err.to_string()
            .contains("no such file: /nonexistent/nope.db"),
        "{err}"
    );
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
    assert!(err.to_string().contains("not a SQLite database"), "{err}");
    assert!(err.to_string().contains("garbage.db"), "{err}");
}

#[tokio::test]
async fn open_a_directory_is_a_connect_error() {
    let dir = tempfile::tempdir().unwrap();
    let err = DbPool::open_sqlite(dir.path())
        .await
        .err()
        .expect("opening a directory must fail");
    assert!(err.to_string().contains("not a file"), "{err}");
}

#[tokio::test]
async fn open_empty_file_succeeds_like_sqlite_itself() {
    // The pre-flight header check lets a zero-length file through on
    // the claim that SQLite treats one as a valid empty database. This holds
    // the driver to that claim: were it to stop being true, the check would be
    // accepting a file the app then fails to open.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("fresh.db");
    std::fs::write(&path, b"").unwrap();
    let pool = DbPool::open_sqlite(&path)
        .await
        .expect("SQLite opens a zero-length file as an empty database");
    assert!(
        pool.introspect().await.unwrap().is_empty(),
        "an empty database has no objects"
    );
    pool.close().await;
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

/// A database holding objects SQLite cannot resolve on hubro's connection:
/// a virtual table whose module hubro's SQLite lacks, a view over it, a view
/// calling a function nothing registered (what a SpatiaLite `ST_*` or sqlean
/// call is without the extension loaded), and a view over a table dropped
/// after the view was created — beside ordinary
/// objects that must come through intact, and a virtual table whose module
/// *is* built in.
///
/// The missing-module table is written straight into `sqlite_master`, since
/// no connection can `CREATE VIRTUAL TABLE` with a module it lacks. The row is
/// the one SQLite itself writes: the same shape was produced with the
/// `sqlite3` CLI's `zipfile` module (absent from the bundled SQLite sqlx
/// links), and opening that file failed introspection with the same
/// `(code: 1) no such module: zipfile` this one fails with as `nosuchmod`.
const UNRESOLVABLE_SCHEMA: &str = "
    CREATE TABLE artists (id INTEGER PRIMARY KEY, name TEXT NOT NULL);
    CREATE INDEX idx_artists_name ON artists(name);
    CREATE TABLE albums (id INTEGER PRIMARY KEY, artist_id INTEGER REFERENCES artists(id));
    CREATE VIRTUAL TABLE docs USING fts5(body);
    CREATE TABLE gone (x INTEGER);
    CREATE VIEW over_gone AS SELECT x FROM gone;
    DROP TABLE gone;
    CREATE VIEW over_func AS SELECT nosuchfunc(1) AS x;
    PRAGMA writable_schema = ON;
    INSERT INTO sqlite_master (type, name, tbl_name, rootpage, sql)
        VALUES ('table', 'shapes', 'shapes', 0, 'CREATE VIRTUAL TABLE shapes USING nosuchmod(geom)');
    INSERT INTO sqlite_master (type, name, tbl_name, rootpage, sql)
        VALUES ('view', 'over_shapes', 'over_shapes', 0, 'CREATE VIEW over_shapes AS SELECT geom FROM shapes');
    PRAGMA writable_schema = OFF;
";

#[tokio::test]
async fn an_object_sqlite_cannot_resolve_costs_only_itself() {
    let fixture = FixtureDb::with_sql(UNRESOLVABLE_SCHEMA).await;
    let pool = fixture.open().await;

    // The whole tree used to fail with the first unresolvable object's error.
    let tables = pool
        .introspect()
        .await
        .expect("one unresolvable object must not fail introspection");

    // Everything resolvable comes through whole — columns, indexes and
    // foreign keys, i.e. every batched catalog read survived.
    let artists = table(&tables, "artists");
    assert_eq!(artists.columns.len(), 2);
    assert_eq!(artists.indexes[0].name, "idx_artists_name");
    assert_eq!(artists.restriction, None);
    let albums = table(&tables, "albums");
    assert_eq!(albums.foreign_keys[0].referenced_table, "artists");
    assert_eq!(albums.restriction, None);
    // A virtual table whose module *is* built in is an ordinary readable
    // object: being virtual is not what makes an object unreadable.
    let docs = table(&tables, "docs");
    let doc_columns: Vec<&str> = docs.columns.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(doc_columns, ["body"]);
    assert_eq!(docs.restriction, None);

    // The unresolvable ones are still listed, with no columns, and declared
    // unreadable so the grid explains instead of running a query that can
    // only fail the same way.
    for name in ["shapes", "over_shapes", "over_func", "over_gone"] {
        let meta = table(&tables, name);
        assert!(meta.columns.is_empty(), "{name}: {:?}", meta.columns);
        assert!(
            matches!(meta.restriction, Some(Restriction::NoRows(_))),
            "{name}: {:?}",
            meta.restriction
        );
        assert!(hubro::db::unreadable_reason(meta).is_some(), "{name}");
    }
    assert_eq!(table(&tables, "shapes").kind, TableKind::Table);
    assert_eq!(table(&tables, "over_gone").kind, TableKind::View);
    // The reason names what kind of thing went wrong.
    assert!(
        unreadable(&tables, "shapes").contains("virtual table"),
        "{}",
        unreadable(&tables, "shapes")
    );
    assert!(
        !unreadable(&tables, "over_gone").contains("virtual table here"),
        "{}",
        unreadable(&tables, "over_gone")
    );

    // Its stored definition is still one click away, since that reads only
    // `sqlite_master`.
    let ddl = pool
        .fetch_ddl(table(&tables, "shapes"), &hubro::db::DdlObject::Object)
        .await
        .unwrap();
    assert_eq!(
        ddl.sql,
        "CREATE VIRTUAL TABLE shapes USING nosuchmod(geom);"
    );

    pool.close().await;
}

/// The message an object declares as its reason for having no rows to show.
fn unreadable(tables: &[TableMeta], name: &str) -> &'static str {
    hubro::db::unreadable_reason(table(tables, name))
        .unwrap_or_else(|| panic!("{name} is not declared unreadable"))
}

#[tokio::test]
async fn tables_whose_names_merely_resemble_sqlite_internals_are_listed() {
    // `_` in a LIKE pattern is any one character and LIKE ignores ASCII
    // case, so an unescaped `sqlite_%` hid all three of these user tables.
    let fixture = FixtureDb::with_sql(
        "CREATE TABLE sqlitest (id INTEGER PRIMARY KEY AUTOINCREMENT, note TEXT);
         CREATE TABLE \"SQLiteXData\" (x INTEGER);
         CREATE INDEX idx_xdata ON \"SQLiteXData\"(x);
         CREATE TABLE \"sqlite-log\" (id INTEGER REFERENCES sqlitest(id));
         PRAGMA writable_schema = ON;
         CREATE TABLE \"SQLITE_Upper\" (x INTEGER);
         PRAGMA writable_schema = OFF;
         INSERT INTO sqlitest (note) VALUES ('a');
         ANALYZE;",
    )
    .await;
    let pool = fixture.open().await;
    let tables = pool.introspect().await.unwrap();

    let names: Vec<&str> = tables.iter().map(|t| t.name.as_str()).collect();
    // SQLite's own bookkeeping (`sqlite_sequence` from AUTOINCREMENT,
    // `sqlite_stat1` from ANALYZE) stays hidden, and so does a reserved name
    // in another case: SQLite reserves the prefix case-insensitively and only
    // creates one like `SQLITE_Upper` under `writable_schema`.
    assert_eq!(names, ["SQLiteXData", "sqlite-log", "sqlitest"]);
    // And every batched catalog read sees them too, not just the object list.
    assert_eq!(table(&tables, "sqlitest").columns.len(), 2);
    let index = &table(&tables, "SQLiteXData").indexes[0];
    assert_eq!(index.name, "idx_xdata");
    assert_eq!(index.columns, ["x"]);
    assert_eq!(
        table(&tables, "sqlite-log").foreign_keys[0].referenced_table,
        "sqlitest"
    );

    pool.close().await;
}

#[tokio::test]
async fn capabilities_are_full_on_tables_and_reduced_on_views() {
    let fixture = FixtureDb::full().await;
    let pool = fixture.open().await;
    let tables = pool.introspect().await.unwrap();

    // SQLite is a full-featured engine: the connection declares everything.
    assert_eq!(pool.backend_capabilities(), Capabilities::FULL);

    // Ordinary tables resolve to the connection's full capability, with no
    // reason to state.
    for name in ["artists", "albums", "settings", "order"] {
        let access = pool.backend_access(table(&tables, name));
        assert_eq!(access.caps, Capabilities::FULL, "{name}");
        assert_eq!(access.restriction, None, "{name}");
        assert!(access.identity.is_some(), "{name}");
    }

    // The view resolves narrower — and says why — while staying readable.
    let view = pool.backend_access(table(&tables, "artist_overview"));
    assert!(!view.can_mutate());
    assert_eq!(view.restriction, Some(Restriction::View));
    assert_eq!(view.read_only_notice(), Some("Views are read-only."));
    assert!(view.caps.read_query);
    assert!(view.caps.offset_paging);

    pool.close().await;
}

#[tokio::test]
async fn a_view_refuses_staged_writes_with_the_stated_reason() {
    let fixture = FixtureDb::full().await;
    let pool = fixture.open().await;
    let tables = pool.introspect().await.unwrap();
    let view = table(&tables, "artist_overview");

    // The UI never offers this, so reaching apply_staged means a gate was
    // missed: it must refuse rather than build SQL against the view.
    let err = apply_staged(
        &pool,
        &pool.backend_access(view),
        view,
        &RowIdentity::PrimaryKey {
            columns: vec!["id".into()],
        },
        &[StagedChange::Update {
            locator: RowLocator {
                identity_values: vec![Value::Integer(1)],
            },
            column: "name".into(),
            value: Value::Text("nope".into()),
        }],
    )
    .await
    .expect_err("a view must refuse staged writes");
    assert!(
        err.to_string().contains("Views are read-only."),
        "unexpected message: {err}"
    );

    // Nothing was sent, so the underlying data is untouched.
    let rows = pool
        .query("SELECT name FROM artist_overview WHERE id = 1")
        .await
        .unwrap();
    assert_eq!(rows.rows[0][0], Value::Text("Ana".into()));

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
async fn query_with_no_rows_yields_empty_result_with_its_headers() {
    let fixture = FixtureDb::full().await;
    let pool = fixture.open().await;

    // A zero-row result still names its columns: the grid shows the
    // shape of what was asked for rather than a blank pane, which is also what
    // the SQL Server backend has always done (TDS sends result-set metadata
    // even for zero rows). The sqlx backends have no row to read the names
    // off, so they fall back to describing the statement.
    let empty = pool
        .query("SELECT artist_id, title FROM albums WHERE 0")
        .await
        .unwrap();
    assert!(empty.rows.is_empty());
    let names: Vec<&str> = empty.columns.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, ["artist_id", "title"]);

    // Same contract on the capped/streaming path the SQL editor uses.
    let (empty, truncated) = pool
        .query_capped("SELECT artist_id, title FROM albums WHERE 0", &[], 100)
        .await
        .unwrap();
    assert!(empty.rows.is_empty());
    assert!(!truncated);
    let names: Vec<&str> = empty.columns.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, ["artist_id", "title"]);

    pool.close().await;
}

#[tokio::test]
async fn statements_without_a_result_set_report_no_columns() {
    let fixture = FixtureDb::full().await;
    let pool = fixture.open().await;

    // The header fallback describes the statement, and a statement with no
    // result set has no columns to describe — that must stay an ordinary empty
    // result rather than an error. `DROP TABLE` is the sharp case: re-preparing
    // it after it ran fails outright ("no such table"), and a statement that
    // already succeeded must not be turned into a failure just to explain a
    // missing header.
    pool.execute("CREATE TABLE headerless (x INTEGER)")
        .await
        .unwrap();
    let (result, truncated) = pool
        .query_capped("DROP TABLE headerless", &[], 100)
        .await
        .unwrap();
    assert!(result.rows.is_empty());
    assert!(result.columns.is_empty());
    assert!(!truncated);

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

#[tokio::test]
async fn sqlite_reports_rows_only_after_analyze_and_never_reports_a_size() {
    let fixture = FixtureDb::full().await;
    let pool = fixture.open().await;
    let tables = pool.introspect().await.unwrap();
    let artists = table(&tables, "artists").clone();

    // A database nobody has analyzed has no `sqlite_stat1` at all — SQLite
    // keeps no statistics of its own. Absent, not zero, and not an error
    // either: selecting from the missing table would be one.
    let before = pool.fetch_table_stats(&artists).await.unwrap();
    assert!(
        before.is_empty(),
        "an un-analyzed database knows nothing: {before:?}"
    );

    let exact = pool.count_table_rows(&artists).await.unwrap();
    assert!(exact > 0, "the fixture seeds artists");

    pool.query("ANALYZE").await.unwrap();
    let after = pool.fetch_table_stats(&artists).await.unwrap();
    assert_eq!(
        after.rows,
        Some(RowCount::Estimated(exact)),
        "ANALYZE writes the row count sqlite_stat1 leads with"
    );
    // Deliberately absent on every SQLite database, analyzed or not: the only
    // per-table size SQLite offers is `dbstat`, which walks every page — the
    // cost this estimate exists to avoid.
    assert_eq!(after.bytes, None, "SQLite reports no per-table size");

    // A view has no stat1 row of its own, so it stays absent while the table
    // it reads from does not.
    let view = pool
        .fetch_table_stats(table(&tables, "artist_overview"))
        .await
        .unwrap();
    assert!(view.is_empty(), "a view has no stored statistics: {view:?}");

    // Quoted and keyword identifiers go through the same qualifier as
    // everything else.
    assert_eq!(
        pool.count_table_rows(table(&tables, "we\"ird table"))
            .await
            .unwrap(),
        3
    );
    pool.close().await;
}

#[tokio::test]
async fn sqlite_explains_a_query_as_raw_output() {
    let fixture = FixtureDb::full().await;
    let pool = fixture.open().await;

    // SQLite gets the degrade path: a dialect-appropriate EXPLAIN,
    // shown verbatim. Only PostgreSQL's JSON is parsed into a tree, and
    // claiming otherwise here would render a plan hubro invented.
    let support = pool
        .explain_support()
        .expect("sqlite has EXPLAIN QUERY PLAN");
    assert!(!support.structured);

    let sql = explain_statement("SELECT * FROM artists WHERE name = 'x'", support);
    assert_eq!(
        sql,
        "EXPLAIN QUERY PLAN SELECT * FROM artists WHERE name = 'x'"
    );
    let result = pool.query(&sql).await.unwrap();
    let PlanDisplay::Raw(text) = PlanDisplay::from_result(support.structured, &result) else {
        panic!("only postgres produces a parsed tree");
    };
    // The server's own words, with the column names that make them readable.
    assert!(text.starts_with("id | parent | notused | detail"), "{text}");
    assert!(text.contains("artists"), "{text}");
    assert!(text.contains("SCAN") || text.contains("SEARCH"), "{text}");

    // And the statement was explained, not run: EXPLAIN QUERY PLAN of a
    // DELETE reports the plan and leaves the rows alone. hubro never adds
    // ANALYZE, and SQLite's EXPLAIN has no such option to add.
    let before = pool.query("SELECT count(*) FROM artists").await.unwrap();
    pool.query(&explain_statement("DELETE FROM artists", support))
        .await
        .unwrap();
    let after = pool.query("SELECT count(*) FROM artists").await.unwrap();
    assert_eq!(before.rows, after.rows);

    pool.close().await;
}

/// `fut`, or a failure naming `what` once it has run for ten seconds. A NUL
/// that reaches sqlx-sqlite never comes back — its worker thread spins
/// forever — so without this a regression hangs the suite instead of
/// failing it. The spinning thread outlives the timeout and dies with the
/// test binary.
async fn within<T>(what: &str, fut: impl std::future::Future<Output = T>) -> T {
    tokio::time::timeout(std::time::Duration::from_secs(10), fut)
        .await
        .unwrap_or_else(|_| panic!("{what} hung: the NUL reached the driver"))
}

fn nul_refused() -> DbError {
    DbError::Query(NUL_IN_SQL.to_string())
}

/// Every way SQL text reaches SQLite outside the script runner, handed a
/// statement that is complete before its NUL — the shape that hangs
/// sqlx-sqlite (measured: `SELECT 1` followed by a NUL spun a core until
/// killed). Each is refused with a sentence, promptly, and nothing ran.
#[tokio::test]
async fn sql_text_holding_a_nul_is_refused_at_every_entry_point() {
    let fixture =
        FixtureDb::with_sql("CREATE TABLE t (id INTEGER PRIMARY KEY); INSERT INTO t VALUES (1)")
            .await;
    let pool = fixture.open().await;
    let read = "SELECT * FROM t;\0";
    let write = "DELETE FROM t;\0";

    assert_eq!(within("query", pool.query(read)).await, Err(nul_refused()));
    assert_eq!(
        within("execute", pool.execute(write)).await,
        Err(nul_refused())
    );
    assert_eq!(
        within("query_capped", pool.query_capped(read, &[], 10)).await,
        Err(nul_refused())
    );
    assert_eq!(
        within("execute_checked", pool.execute_checked(write, &[], 1)).await,
        Err(nul_refused())
    );
    let mut out = Vec::new();
    assert_eq!(
        within(
            "export",
            pool.export(read, &[], ExportFormat::Csv, &mut out)
        )
        .await,
        Err(nul_refused())
    );
    assert!(out.is_empty(), "a refused export writes nothing");

    // The transaction a script or an import runs in.
    let mut tx = pool.begin_script_tx().await.unwrap();
    assert_eq!(
        within("tx execute", tx.execute(write)).await,
        Err(nul_refused())
    );
    assert_eq!(
        within("tx execute_with", tx.execute_with(write, &[])).await,
        Err(nul_refused())
    );
    assert_eq!(
        within("tx query_capped", tx.query_capped(read, 10)).await,
        Err(nul_refused())
    );
    let _ = tx.rollback().await;

    // Nothing ran, and the pool is whole: a hung statement would also have
    // kept its connection.
    let rows = within("count", pool.query("SELECT count(*) FROM t")).await;
    assert_eq!(rows.unwrap().rows, vec![vec![Value::Integer(1)]]);
    pool.close().await;
}

/// A table whose `sqlite_master` name holds a NUL. A crafted file can carry
/// one, and SQLite opens it: it compares names as C strings, so `t` NUL `x`
/// is still the table `t` to it (measured, and `integrity_check` says ok).
/// hubro reads the whole name, so it is listed, and every statement built
/// from it carries the NUL. Inside quotes that fails rather than hangs, but
/// with `unrecognized token` — the refusal says what is actually wrong, and
/// does not depend on every builder quoting.
#[tokio::test]
async fn a_table_whose_catalog_name_holds_a_nul_is_refused_not_hung() {
    let fixture = FixtureDb::with_sql(
        "CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT); INSERT INTO t VALUES (1, 'x'); \
         PRAGMA writable_schema = ON; \
         UPDATE sqlite_master SET name = 't' || char(0) || 'x' WHERE name = 't'; \
         PRAGMA writable_schema = OFF;",
    )
    .await;
    let pool = fixture.open().await;
    // Introspection binds names rather than building text from them, so the
    // table is listed whole, with the columns SQLite resolves for `t`.
    let tables = within("introspect", pool.introspect()).await.unwrap();
    let crafted = table(&tables, "t\0x").clone();
    let columns: Vec<&str> = crafted.columns.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(columns, ["id", "v"]);

    let request = PageRequest {
        schema: None,
        table: crafted.name.clone(),
        limit: 10,
        offset: 0,
        sort: None,
        filter: None,
        extra_key_column: None,
    };
    assert_eq!(
        within("fetch_page", pool.fetch_page(&request)).await,
        Err(nul_refused())
    );
    assert_eq!(
        within(
            "fetch_page_bounded",
            pool.fetch_page_bounded(&request, &crafted.columns, &["id"])
        )
        .await
        .err(),
        Some(nul_refused())
    );
    assert_eq!(
        within("count_rows", pool.count_rows(&request)).await,
        Err(nul_refused())
    );
    assert_eq!(
        within("count_table_rows", pool.count_table_rows(&crafted)).await,
        Err(nul_refused())
    );
    let (sql, params) = request.export_sql(pool.dialect());
    let mut out = Vec::new();
    assert_eq!(
        within(
            "export",
            pool.export(&sql, &params, ExportFormat::Json, &mut out)
        )
        .await,
        Err(nul_refused())
    );
    let identity = RowIdentity::PrimaryKey {
        columns: vec!["id".into()],
    };
    let err = within(
        "apply_staged",
        apply_staged(
            &pool,
            &pool.backend_access(&crafted),
            &crafted,
            &identity,
            &[StagedChange::Delete {
                locator: RowLocator {
                    identity_values: vec![Value::Integer(1)],
                },
            }],
        ),
    )
    .await
    .expect_err("a delete built from the name must be refused");
    assert_eq!(err.message, nul_refused().to_string(), "{err}");

    // The row is still there, read under the name SQLite knows.
    let rows = within("count", pool.query("SELECT count(*) FROM t")).await;
    assert_eq!(rows.unwrap().rows, vec![vec![Value::Integer(1)]]);
    pool.close().await;
}

/// Every primary-key shape SQLite has that could be mistaken for the rowid
/// alias, or the alias written in a less obvious way: `(table, column
/// definitions, table suffix, whether SQLite fills a NULL id)`. The last
/// field is what SQLite 3.46 was measured to do; the test measures it again
/// rather than trusting it, so a different SQLite fails here, not in a user's
/// import.
const ROWID_SHAPES: &[(&str, &str, &str, bool)] = &[
    ("alias_plain", "id INTEGER PRIMARY KEY, x TEXT", "", true),
    (
        "alias_not_null",
        "id INTEGER PRIMARY KEY NOT NULL, x TEXT",
        "",
        true,
    ),
    (
        "alias_lowercase",
        "id integer primary key, x TEXT",
        "",
        true,
    ),
    (
        "alias_quoted_type",
        "id \"INTEGER\" PRIMARY KEY NOT NULL, x TEXT",
        "",
        true,
    ),
    (
        "alias_table_pk",
        "id INTEGER NOT NULL, x TEXT, PRIMARY KEY(id)",
        "",
        true,
    ),
    // The table-constraint DESC form is still the alias…
    (
        "alias_table_pk_desc",
        "id INTEGER NOT NULL, x TEXT, PRIMARY KEY(id DESC)",
        "",
        true,
    ),
    (
        "alias_autoincrement",
        "id INTEGER PRIMARY KEY AUTOINCREMENT NOT NULL, x TEXT",
        "",
        true,
    ),
    (
        "alias_strict",
        "id INTEGER PRIMARY KEY NOT NULL, x TEXT",
        " STRICT",
        true,
    ),
    // …and the column-constraint DESC form is not (SQLite's documented quirk).
    (
        "column_pk_desc",
        "id INTEGER PRIMARY KEY DESC NOT NULL, x TEXT",
        "",
        false,
    ),
    ("int_pk", "id INT PRIMARY KEY NOT NULL, x TEXT", "", false),
    (
        "bigint_pk",
        "id BIGINT PRIMARY KEY NOT NULL, x TEXT",
        "",
        false,
    ),
    (
        "integer4_pk",
        "id INTEGER(4) PRIMARY KEY NOT NULL, x TEXT",
        "",
        false,
    ),
    ("text_pk", "id TEXT PRIMARY KEY NOT NULL, x TEXT", "", false),
    (
        "strict_int_pk",
        "id INT PRIMARY KEY NOT NULL, x TEXT",
        " STRICT",
        false,
    ),
    (
        "without_rowid",
        "id INTEGER PRIMARY KEY NOT NULL, x TEXT",
        " WITHOUT ROWID",
        false,
    ),
    // A WITHOUT ROWID key is NOT NULL even when it doesn't say so.
    (
        "without_rowid_implicit",
        "id INTEGER PRIMARY KEY, x TEXT",
        " WITHOUT ROWID",
        false,
    ),
    (
        "composite",
        "id INTEGER NOT NULL, b INTEGER NOT NULL, x TEXT, PRIMARY KEY(id, b)",
        "",
        false,
    ),
];

/// Whether the statement ran and left `id` holding an integer SQLite chose —
/// not a NULL it stored (a legacy nullable non-alias key takes one), and not
/// a refusal.
async fn assigns_an_id(pool: &DbPool, table: &str, insert: &str) -> bool {
    if pool.execute(insert).await.is_err() {
        return false;
    }
    let rows = pool
        .query(&format!("SELECT typeof(id) FROM {table} WHERE x = 'probe'"))
        .await
        .unwrap()
        .rows;
    pool.execute(&format!("DELETE FROM {table}")).await.unwrap();
    rows == vec![vec![Value::Text("integer".into())]]
}

#[tokio::test]
async fn the_rowid_alias_is_database_filled_exactly_where_sqlite_fills_it() {
    let fixture = FixtureDb::with_sql("").await;
    let pool = fixture.open().await;
    for (table, columns, suffix, _) in ROWID_SHAPES {
        pool.execute(&format!("CREATE TABLE {table} ({columns}){suffix}"))
            .await
            .unwrap();
    }
    let tables = pool.introspect().await.unwrap();

    for (name, columns, _, alias) in ROWID_SHAPES {
        let (extra_col, extra_val) = if columns.contains(" b ") {
            (", b", ", 1")
        } else {
            ("", "")
        };
        let null_fills = assigns_an_id(
            &pool,
            name,
            &format!("INSERT INTO {name} (id{extra_col}, x) VALUES (NULL{extra_val}, 'probe')"),
        )
        .await;
        let omission_fills = assigns_an_id(
            &pool,
            name,
            &format!("INSERT INTO {name} (x{extra_col}) VALUES ('probe'{extra_val})"),
        )
        .await;
        assert_eq!(null_fills, *alias, "{name}: SQLite itself changed");
        assert_eq!(omission_fills, *alias, "{name}: SQLite itself changed");

        let id = table(&tables, name)
            .columns
            .iter()
            .find(|c| c.name == "id")
            .unwrap();
        // What import and the new-row form act on: a column the database
        // fills is never demanded of the user, and a blank becomes an omission.
        assert_eq!(id.is_auto_assigned(), *alias, "{name}: {id:?}");
        let expected = if *alias {
            Generated::RowidAlias
        } else {
            Generated::Never
        };
        assert_eq!(id.generated, expected, "{name}");
        if *alias {
            // The alias can never hold NULL — `UPDATE … SET id = NULL` is a
            // datatype mismatch — whatever its declaration says.
            assert!(!id.nullable, "{name}: {id:?}");
        }
    }
    pool.close().await;
}

#[tokio::test]
async fn an_update_cannot_store_null_in_the_rowid_alias() {
    // Why introspection reports the alias NOT NULL even when its declaration
    // omits the constraint: the editor would otherwise offer a NULL the
    // engine refuses.
    let fixture = FixtureDb::with_sql("").await;
    let pool = fixture.open().await;
    pool.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, x TEXT)")
        .await
        .unwrap();
    pool.execute("INSERT INTO t (x) VALUES ('a')")
        .await
        .unwrap();
    let err = pool
        .execute("UPDATE t SET id = NULL")
        .await
        .expect_err("the rowid alias cannot hold NULL");
    assert!(err.to_string().contains("datatype mismatch"), "{err}");
    pool.close().await;
}
