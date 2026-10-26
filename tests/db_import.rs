//! End-to-end tests for CSV/JSON import: `run_import` streams a
//! file into an existing table inside ONE transaction, aborting and rolling
//! everything back on the first row it cannot coerce — unless skip mode was
//! chosen up front, in which case the bad rows are reported by line and the
//! rest lands.
//!
//! The rollback is the feature's whole safety claim, so it is tested from the
//! side that can actually fail: a file whose *last* batch is bad, against a
//! table that already holds rows, asserting both that the table is
//! byte-for-byte unchanged and that the import really had inserted rows
//! before undoing them (`ImportError::undone_rows`). A batch size small
//! enough to force several round trips is what makes that a real test rather
//! than a statement about one statement.
//!
//! SQLite cases always run. Postgres cases need `HUBRO_PG_TEST_URL` and SQL
//! Server cases `HUBRO_MSSQL_TEST_URL` — see tests/db_postgres.rs and
//! tests/db_sqlserver.rs for the docker run recipes.

mod common;

use hubro::db::Reading;
use std::io::BufReader;

use common::FixtureDb;
use hubro::db::{
    run_import, run_script, split_statements, ColumnBinding, CsvDialect, CsvReader, DbPool,
    Dialect, EmptyField, Encoding, ErrorMode, ImportError, ImportOptions, ImportReport, JsonReader,
    JsonShape, PageRequest, PgFlavor, RecordSource, SourceField, TableAccess, TableMeta, Value,
    INSTEAD_OF_TRIGGER,
};

async fn pg_url() -> Option<String> {
    common::pg_test_url().await
}

async fn mssql_url() -> Option<String> {
    common::mssql_test_url().await
}

fn find<'a>(tables: &'a [TableMeta], name: &str) -> &'a TableMeta {
    tables
        .iter()
        .find(|t| t.name == name)
        .unwrap_or_else(|| panic!("no table {name}"))
}

fn bind(index: usize, column: &str) -> ColumnBinding {
    ColumnBinding {
        source: SourceField::Index(index),
        column: column.to_string(),
    }
}

fn key(key: &str, column: &str) -> ColumnBinding {
    ColumnBinding {
        source: SourceField::Key(key.to_string()),
        column: column.to_string(),
    }
}

/// A CSV reader over an in-memory file with a header row.
fn csv(text: &str) -> Box<dyn RecordSource + '_> {
    Box::new(CsvReader::new(
        BufReader::new(text.as_bytes()),
        CsvDialect::default(),
        Encoding::Utf8,
    ))
}

fn json_array(text: &str) -> Box<dyn RecordSource + '_> {
    Box::new(JsonReader::new(
        BufReader::new(text.as_bytes()),
        JsonShape::Array,
        Encoding::Utf8,
    ))
}

fn options(mapping: Vec<ColumnBinding>, on_error: ErrorMode) -> ImportOptions {
    ImportOptions {
        mapping,
        empty_field: EmptyField::Null,
        on_error,
    }
}

/// Every row of `table`, ordered by its first column, as display text — the
/// "is the table exactly what it was" comparison the rollback tests need.
async fn snapshot(pool: &DbPool, table: &TableMeta, order_by: &str) -> Vec<Vec<String>> {
    let sql = format!(
        "SELECT * FROM {} ORDER BY {}",
        match &table.schema {
            Some(schema) => format!("\"{schema}\".\"{}\"", table.name),
            None => format!("\"{}\"", table.name),
        },
        format_args!("\"{order_by}\"")
    );
    let result = pool.query_unbounded(&sql).await.unwrap();
    result
        .rows
        .iter()
        .map(|row| row.iter().map(Value::display).collect())
        .collect()
}

async fn import(
    pool: &DbPool,
    table: &TableMeta,
    options: &ImportOptions,
    source: &mut dyn RecordSource,
) -> Result<ImportReport, ImportError> {
    let access = pool.backend_access(table);
    run_import(pool, &access, table, options, source).await
}

/// The import's transaction gives its connection back to the pool, unlike a
/// script's, which is closed so the user's session state goes with it. The
/// import runs only `INSERT`s hubro builds, so there is nothing to discard,
/// and closing would cost a reconnect — and, cancelled, a close that sqlx
/// abandons after five seconds while a SQLite batch could still hold its
/// write lock. On a pool of one connection a temp table marks the session: it
/// is still there after the import only if the import ran on, and returned,
/// that same connection. Both endings are covered — a commit, and the
/// rollback of an import that fails.
#[tokio::test]
async fn sqlite_import_returns_its_connection_to_the_pool() {
    let fixture = FixtureDb::with_sql(
        "CREATE TABLE people (id INTEGER PRIMARY KEY, name TEXT NOT NULL, weight REAL)",
    )
    .await;
    let options_ = sqlx::sqlite::SqliteConnectOptions::new().filename(fixture.path());
    let pool = DbPool::Sqlite(
        sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(options_)
            .await
            .unwrap(),
    );
    pool.execute("CREATE TEMP TABLE session_marker (a)")
        .await
        .unwrap();
    let tables = pool.introspect().await.unwrap();
    let people = find(&tables, "people");
    let mapping = || vec![bind(0, "id"), bind(1, "name"), bind(2, "weight")];
    let marker = "SELECT count(*) FROM sqlite_temp_master WHERE name = 'session_marker'";

    let report = import(
        &pool,
        people,
        &options(mapping(), ErrorMode::Abort),
        csv("id,name,weight\n1,ada,1.5\n").as_mut(),
    )
    .await
    .unwrap();
    assert_eq!(report.inserted_rows, 1);
    assert_eq!(
        pool.query_unbounded(marker).await.unwrap().rows,
        vec![vec![Value::Integer(1)]],
        "a committed import closed its connection instead of returning it"
    );

    import(
        &pool,
        people,
        &options(mapping(), ErrorMode::Abort),
        csv(&csv_with_a_bad_row(5, 5)).as_mut(),
    )
    .await
    .expect_err("the last row cannot be coerced");
    assert_eq!(
        pool.query_unbounded(marker).await.unwrap().rows,
        vec![vec![Value::Integer(1)]],
        "a rolled-back import closed its connection instead of returning it"
    );
    pool.close().await;
}

/// A CSV whose rows are fine up to `bad_at`, where one row carries a value
/// the target column cannot take. Long enough to span several batches so a
/// failure has real, already-inserted rows behind it.
fn csv_with_a_bad_row(rows: usize, bad_at: usize) -> String {
    let mut text = String::from("id,name,weight\n");
    for i in 1..=rows {
        if i == bad_at {
            text.push_str(&format!("{i},row{i},not-a-number\n"));
        } else {
            text.push_str(&format!("{i},row{i},{}.5\n", i));
        }
    }
    text
}

/// An import of `text` that fails at `line` on a value its target cannot
/// take, after at least `undone_at_least` rows of it were already inserted,
/// reports that line and undoes every one of them, leaving `table` exactly
/// as it was. Its message names the column and the value.
async fn assert_a_bad_file_rolls_back(
    pool: &DbPool,
    table: &TableMeta,
    mapping: Vec<ColumnBinding>,
    text: &str,
    line: u64,
    undone_at_least: u64,
) {
    let before = snapshot(pool, table, "id").await;
    let rows_before = pool
        .count_table_rows(Reading::UNMARKED, table)
        .await
        .unwrap();
    let err = import(
        pool,
        table,
        &options(mapping, ErrorMode::Abort),
        csv(text).as_mut(),
    )
    .await
    .unwrap_err();
    assert_eq!(err.line, Some(line), "the header is line 1");
    assert!(err.message.contains("weight"), "{}", err.message);
    assert!(err.message.contains("not-a-number"), "{}", err.message);
    assert!(
        err.undone_rows >= undone_at_least,
        "rows must really have been inserted before the rollback, got {}",
        err.undone_rows
    );
    // ...and the table is exactly what it was.
    assert_eq!(snapshot(pool, table, "id").await, before);
    assert_eq!(
        pool.count_table_rows(Reading::UNMARKED, table)
            .await
            .unwrap(),
        rows_before
    );
}

#[tokio::test]
async fn sqlite_csv_import_lands_every_row_in_one_transaction() {
    let fixture = FixtureDb::with_sql("").await;
    let pool = fixture.open().await;
    pool.execute("CREATE TABLE people (id INTEGER PRIMARY KEY, name TEXT NOT NULL, weight REAL)")
        .await
        .unwrap();
    let tables = pool.introspect().await.unwrap();
    let people = find(&tables, "people");

    let text = "id,name,weight\n1,ada,1.5\n2,grace,\n3,alan,3.25\n";
    let report = import(
        &pool,
        people,
        &options(
            vec![bind(0, "id"), bind(1, "name"), bind(2, "weight")],
            ErrorMode::Abort,
        ),
        csv(text).as_mut(),
    )
    .await
    .unwrap();

    assert_eq!(report.inserted_rows, 3);
    assert_eq!(report.skipped_rows, 0);
    assert_eq!(
        snapshot(&pool, people, "id").await,
        vec![
            vec!["1".to_string(), "ada".into(), "1.5".into()],
            // The empty field became NULL, per the default option.
            vec!["2".to_string(), "grace".into(), "NULL".into()],
            vec!["3".to_string(), "alan".into(), "3.25".into()],
        ]
    );
}

#[tokio::test]
async fn sqlite_a_bad_row_rolls_back_every_row_already_inserted() {
    let fixture = FixtureDb::with_sql("").await;
    let pool = fixture.open().await;
    pool.execute("CREATE TABLE people (id INTEGER PRIMARY KEY, name TEXT NOT NULL, weight REAL)")
        .await
        .unwrap();
    pool.execute("INSERT INTO people VALUES (0, 'existing', 9.5)")
        .await
        .unwrap();
    let tables = pool.introspect().await.unwrap();
    let people = find(&tables, "people");
    // 1200 rows against a 300-row batch (900 parameter ceiling / 3 columns):
    // the bad row is in the last batch, so three batches have already been
    // sent and committed-to-the-transaction when it fails.
    assert_a_bad_file_rolls_back(
        &pool,
        people,
        vec![bind(0, "id"), bind(1, "name"), bind(2, "weight")],
        &csv_with_a_bad_row(1200, 1150),
        1151,
        900,
    )
    .await;
    // The one row the table started with.
    assert_eq!(
        pool.count_table_rows(Reading::UNMARKED, people)
            .await
            .unwrap(),
        1
    );
}

#[tokio::test]
async fn sqlite_skip_mode_reports_every_bad_row_by_line_and_imports_the_rest() {
    let fixture = FixtureDb::with_sql("").await;
    let pool = fixture.open().await;
    pool.execute("CREATE TABLE people (id INTEGER PRIMARY KEY, name TEXT NOT NULL, weight REAL)")
        .await
        .unwrap();
    let tables = pool.introspect().await.unwrap();
    let people = find(&tables, "people");

    // Line 3: a weight that is not a number. Line 5: a NOT NULL name left
    // empty. Both are hubro's own rejections, which is what skip mode covers.
    let text = "id,name,weight\n\
                1,ada,1.5\n\
                2,grace,heavy\n\
                3,alan,3.5\n\
                4,,4.5\n\
                5,edsger,5.5\n";
    let report = import(
        &pool,
        people,
        &options(
            vec![bind(0, "id"), bind(1, "name"), bind(2, "weight")],
            ErrorMode::Skip,
        ),
        csv(text).as_mut(),
    )
    .await
    .unwrap();

    assert_eq!(report.inserted_rows, 3);
    assert_eq!(report.skipped_rows, 2);
    assert_eq!(
        report.skipped.iter().map(|s| s.line).collect::<Vec<_>>(),
        vec![3, 5],
        "skipped rows are reported by their physical line"
    );
    assert!(
        report.skipped[0].reason.contains("weight"),
        "{:?}",
        report.skipped[0]
    );
    assert!(
        report.skipped[1].reason.contains("name"),
        "{:?}",
        report.skipped[1]
    );
    assert!(!report.skips_truncated());

    assert_eq!(
        snapshot(&pool, people, "id").await,
        vec![
            vec!["1".to_string(), "ada".into(), "1.5".into()],
            vec!["3".to_string(), "alan".into(), "3.5".into()],
            vec!["5".to_string(), "edsger".into(), "5.5".into()],
        ]
    );
}

#[tokio::test]
async fn sqlite_a_windows_file_with_a_trailing_blank_line_imports_exactly_its_rows() {
    // CRLF with a trailing blank line is what Excel writes, i.e. the single
    // most likely file anyone will import. The CR used to count as content,
    // so the blank line became a record of one empty field: an all-NULL row
    // committed here, and against the NOT NULL column below, the whole import
    // aborted at a line with no data on it.
    let fixture = FixtureDb::with_sql("").await;
    let pool = fixture.open().await;
    pool.execute("CREATE TABLE people (id INTEGER PRIMARY KEY, name TEXT NOT NULL, weight REAL)")
        .await
        .unwrap();
    let tables = pool.introspect().await.unwrap();
    let people = find(&tables, "people");

    let text = "id,name,weight\r\n1,ada,1.5\r\n2,grace,2.5\r\n\r\n";
    let report = import(
        &pool,
        people,
        &options(
            vec![bind(0, "id"), bind(1, "name"), bind(2, "weight")],
            ErrorMode::Abort,
        ),
        csv(text).as_mut(),
    )
    .await
    .unwrap();

    assert_eq!(report.inserted_rows, 2, "the blank line is not a row");
    assert_eq!(
        snapshot(&pool, people, "id").await,
        vec![
            vec!["1".to_string(), "ada".into(), "1.5".into()],
            vec!["2".to_string(), "grace".into(), "2.5".into()],
        ]
    );
}

#[tokio::test]
async fn sqlite_json_array_and_ndjson_import_the_same_rows() {
    let fixture = FixtureDb::with_sql("").await;
    let pool = fixture.open().await;
    pool.execute("CREATE TABLE people (id INTEGER PRIMARY KEY, name TEXT NOT NULL, weight REAL)")
        .await
        .unwrap();
    let tables = pool.introspect().await.unwrap();
    let people = find(&tables, "people");
    let mapping = vec![
        key("id", "id"),
        key("name", "name"),
        key("weight", "weight"),
    ];

    let array = r#"[{"id":1,"name":"ada","weight":1.5},{"id":2,"name":"grace","weight":null}]"#;
    let report = import(
        &pool,
        people,
        &options(mapping.clone(), ErrorMode::Abort),
        json_array(array).as_mut(),
    )
    .await
    .unwrap();
    assert_eq!(report.inserted_rows, 2);
    let from_array = snapshot(&pool, people, "id").await;
    // A JSON null is NULL, and a JSON number keeps its type.
    assert_eq!(from_array[1][2], "NULL");
    assert_eq!(from_array[0][2], "1.5");

    pool.execute("DELETE FROM people").await.unwrap();
    let lines = "{\"id\":1,\"name\":\"ada\",\"weight\":1.5}\n\
                 {\"id\":2,\"name\":\"grace\",\"weight\":null}\n";
    let mut source = JsonReader::new(
        BufReader::new(lines.as_bytes()),
        JsonShape::Lines,
        Encoding::Utf8,
    );
    let report = import(
        &pool,
        people,
        &options(mapping, ErrorMode::Abort),
        &mut source,
    )
    .await
    .unwrap();
    assert_eq!(report.inserted_rows, 2);
    assert_eq!(
        snapshot(&pool, people, "id").await,
        from_array,
        "the two JSON shapes must land identically"
    );
}

#[tokio::test]
async fn sqlite_a_read_only_marking_refuses_the_import_before_reading_the_file() {
    let fixture = FixtureDb::with_sql("").await;
    let pool = fixture.open().await;
    pool.execute("CREATE TABLE people (id INTEGER PRIMARY KEY, name TEXT NOT NULL)")
        .await
        .unwrap();
    let tables = pool.introspect().await.unwrap();
    let people = find(&tables, "people");

    let access = TableAccess::resolve_protected(
        pool.backend_capabilities(),
        hubro::db::WriteProtection::ReadOnly,
        people,
        Dialect::Sqlite,
    );
    let err = run_import(
        &pool,
        &access,
        people,
        &options(vec![bind(0, "id"), bind(1, "name")], ErrorMode::Abort),
        csv("id,name\n1,ada\n").as_mut(),
    )
    .await
    .unwrap_err();

    assert!(err.message.contains("marked"), "{}", err.message);
    assert_eq!(err.undone_rows, 0);
    assert_eq!(
        pool.count_table_rows(Reading::UNMARKED, people)
            .await
            .unwrap(),
        0
    );
}

#[tokio::test]
async fn sqlite_import_round_trips_the_export_of_the_same_table() {
    // The strongest end-to-end statement available: export a table, import
    // the file back into an empty copy, and the two must be identical —
    // NULLs, blobs and unicode included.
    let fixture = FixtureDb::full().await;
    let pool = fixture.open().await;
    pool.execute(
        "CREATE TABLE artists_copy (
            id INTEGER PRIMARY KEY,
            name TEXT NOT NULL,
            rating REAL,
            cover BLOB,
            notes TEXT DEFAULT 'none'
        )",
    )
    .await
    .unwrap();
    let tables = pool.introspect().await.unwrap();
    let artists = find(&tables, "artists");
    let copy = find(&tables, "artists_copy");

    let request = PageRequest {
        limit: 0,
        ..common::page("artists".to_string())
    };
    let (sql, params) = request.export_sql(Dialect::Sqlite);
    let mut buffer: Vec<u8> = Vec::new();
    let exported = pool
        .export(
            Reading::UNMARKED,
            &sql,
            &params,
            hubro::db::ExportFormat::Csv,
            &mut buffer,
        )
        .await
        .unwrap();
    let text = String::from_utf8(buffer).unwrap();

    let report = import(
        &pool,
        copy,
        &options(
            vec![
                bind(0, "id"),
                bind(1, "name"),
                bind(2, "rating"),
                bind(3, "cover"),
                bind(4, "notes"),
            ],
            ErrorMode::Abort,
        ),
        csv(text.as_str()).as_mut(),
    )
    .await
    .unwrap();
    assert_eq!(report.inserted_rows, exported);

    let original = snapshot(&pool, artists, "id").await;
    let round_tripped = snapshot(&pool, copy, "id").await;
    assert_eq!(round_tripped, original);
}

async fn pg_fixture(pool: &DbPool, table: &str) {
    pool.execute(&format!("DROP TABLE IF EXISTS {table}"))
        .await
        .unwrap();
    pool.execute(&format!(
        "CREATE TABLE {table} (
            id integer PRIMARY KEY,
            name text NOT NULL,
            weight real,
            active boolean,
            payload jsonb
        )"
    ))
    .await
    .unwrap();
}

#[tokio::test]
async fn postgres_csv_import_coerces_every_column_type() {
    let Some(url) = pg_url().await else { return };
    let pool = DbPool::open_postgres(&url).await.unwrap();
    pg_fixture(&pool, "import_types").await;
    let tables = pool.introspect().await.unwrap();
    let table = find(&tables, "import_types");

    let text = "id,name,weight,active,payload\n\
                1,ada,1.5,true,\"{\"\"a\"\":1}\"\n\
                2,grace,,f,\n";
    let report = import(
        &pool,
        table,
        &options(
            vec![
                bind(0, "id"),
                bind(1, "name"),
                bind(2, "weight"),
                bind(3, "active"),
                bind(4, "payload"),
            ],
            ErrorMode::Abort,
        ),
        csv(text).as_mut(),
    )
    .await
    .unwrap();
    assert_eq!(report.inserted_rows, 2);

    // Every value reached its typed column: text-staged values coerce through
    // the same per-column casts a staged edit uses.
    let rows = snapshot(&pool, table, "id").await;
    assert_eq!(rows[0][3], "true");
    assert_eq!(rows[1][3], "false");
    // The jsonb column round-trips as JSON (asserted by parsing rather than
    // by spelling: what comes back has been through the server's jsonb
    // storage and hubro's own json decoding, neither of which promises the
    // input's exact whitespace).
    let payload: serde_json::Value = serde_json::from_str(&rows[0][4]).unwrap();
    assert_eq!(payload["a"], 1);
    assert_eq!(rows[1][2], "NULL");
    assert_eq!(rows[1][4], "NULL");

    pool.execute("DROP TABLE import_types").await.unwrap();
    pool.close().await;
}

#[tokio::test]
async fn postgres_a_bad_row_rolls_back_every_row_already_inserted() {
    let Some(url) = pg_url().await else { return };
    let pool = DbPool::open_postgres(&url).await.unwrap();
    pool.execute("DROP TABLE IF EXISTS import_rollback")
        .await
        .unwrap();
    pool.execute(
        "CREATE TABLE import_rollback (
            id integer PRIMARY KEY,
            name text NOT NULL,
            weight real
        )",
    )
    .await
    .unwrap();
    pool.execute("INSERT INTO import_rollback VALUES (0, 'existing', 9.5)")
        .await
        .unwrap();
    let tables = pool.introspect().await.unwrap();
    let table = find(&tables, "import_rollback");
    // 60000 parameters / 3 columns is capped at 500 rows per batch, so 1200
    // rows is three batches: the failure has two full batches behind it.
    assert_a_bad_file_rolls_back(
        &pool,
        table,
        vec![bind(0, "id"), bind(1, "name"), bind(2, "weight")],
        &csv_with_a_bad_row(1200, 1150),
        1151,
        1000,
    )
    .await;
    // The one row the table started with.
    assert_eq!(
        pool.count_table_rows(Reading::UNMARKED, table)
            .await
            .unwrap(),
        1
    );

    pool.execute("DROP TABLE import_rollback").await.unwrap();
    pool.close().await;
}

#[tokio::test]
async fn postgres_a_server_side_failure_also_rolls_the_whole_import_back() {
    // The other half of the guarantee: a row hubro accepts but the *server*
    // refuses (a duplicate key) is not skippable in either mode, and must
    // leave nothing behind.
    let Some(url) = pg_url().await else { return };
    let pool = DbPool::open_postgres(&url).await.unwrap();
    pool.execute("DROP TABLE IF EXISTS import_conflict")
        .await
        .unwrap();
    pool.execute("CREATE TABLE import_conflict (id integer PRIMARY KEY, name text)")
        .await
        .unwrap();
    pool.execute("INSERT INTO import_conflict VALUES (7, 'already here')")
        .await
        .unwrap();
    let tables = pool.introspect().await.unwrap();
    let table = find(&tables, "import_conflict");

    for mode in [ErrorMode::Abort, ErrorMode::Skip] {
        let err = import(
            &pool,
            table,
            &options(vec![bind(0, "id"), bind(1, "name")], mode),
            csv("id,name\n1,ada\n7,duplicate\n8,alan\n").as_mut(),
        )
        .await
        .unwrap_err();
        assert!(
            err.message.contains("duplicate key") || err.message.contains("unique"),
            "{mode:?}: {}",
            err.message
        );
        assert_eq!(
            pool.count_table_rows(Reading::UNMARKED, table)
                .await
                .unwrap(),
            1,
            "{mode:?}: nothing may survive a server-side rejection"
        );
    }

    pool.execute("DROP TABLE import_conflict").await.unwrap();
    pool.close().await;
}

#[tokio::test]
async fn postgres_skip_mode_reports_lines_and_commits_the_rest() {
    let Some(url) = pg_url().await else { return };
    let pool = DbPool::open_postgres(&url).await.unwrap();
    pg_fixture(&pool, "import_skip").await;
    let tables = pool.introspect().await.unwrap();
    let table = find(&tables, "import_skip");

    let text = "id,name,weight,active,payload\n\
                1,ada,1.5,true,\n\
                2,grace,heavy,true,\n\
                3,alan,3.5,perhaps,\n\
                4,edsger,4.5,false,\n";
    let report = import(
        &pool,
        table,
        &options(
            vec![
                bind(0, "id"),
                bind(1, "name"),
                bind(2, "weight"),
                bind(3, "active"),
                bind(4, "payload"),
            ],
            ErrorMode::Skip,
        ),
        csv(text).as_mut(),
    )
    .await
    .unwrap();

    assert_eq!(report.inserted_rows, 2);
    assert_eq!(report.skipped_rows, 2);
    assert_eq!(
        report.skipped.iter().map(|s| s.line).collect::<Vec<_>>(),
        vec![3, 4]
    );
    assert_eq!(
        pool.count_table_rows(Reading::UNMARKED, table)
            .await
            .unwrap(),
        2
    );

    pool.execute("DROP TABLE import_skip").await.unwrap();
    pool.close().await;
}

#[tokio::test]
async fn mssql_csv_import_lands_rows_and_rolls_a_bad_file_back() {
    let Some(url) = mssql_url().await else { return };
    let pool = DbPool::open_mssql(&url).await.unwrap();
    pool.execute("IF OBJECT_ID('dbo.import_rows', 'U') IS NOT NULL DROP TABLE dbo.import_rows")
        .await
        .unwrap();
    pool.execute(
        "CREATE TABLE dbo.import_rows (
            id int PRIMARY KEY,
            name nvarchar(100) NOT NULL,
            weight float NULL,
            active bit NULL
        )",
    )
    .await
    .unwrap();
    pool.execute("INSERT INTO dbo.import_rows VALUES (0, N'existing', 9.5, 1)")
        .await
        .unwrap();
    let tables = pool.introspect().await.unwrap();
    let table = find(&tables, "import_rows");
    let mapping = vec![
        bind(0, "id"),
        bind(1, "name"),
        bind(2, "weight"),
        bind(3, "active"),
    ];

    // A good file first: 1200 rows across several batches (2000 parameters /
    // 4 columns = 500 rows, itself the row cap).
    let mut good = String::from("id,name,weight,active\n");
    for i in 1..=1200 {
        good.push_str(&format!("{i},row{i},{i}.5,{}\n", i % 2 == 0));
    }
    let report = import(
        &pool,
        table,
        &options(mapping.clone(), ErrorMode::Abort),
        csv(good.as_str()).as_mut(),
    )
    .await
    .unwrap();
    assert_eq!(report.inserted_rows, 1200);
    assert_eq!(
        pool.count_table_rows(Reading::UNMARKED, table)
            .await
            .unwrap(),
        1201
    );

    // ...then a bad one, which must leave those 1201 rows exactly as they are.
    let mut bad = String::from("id,name,weight,active\n");
    for i in 2001..=3200 {
        if i == 3150 {
            bad.push_str(&format!("{i},row{i},not-a-number,1\n"));
        } else {
            bad.push_str(&format!("{i},row{i},{i}.5,1\n"));
        }
    }
    assert_a_bad_file_rolls_back(&pool, table, mapping, &bad, 1151, 1000).await;
    assert_eq!(
        pool.count_table_rows(Reading::UNMARKED, table)
            .await
            .unwrap(),
        1201
    );

    pool.execute("DROP TABLE dbo.import_rows").await.unwrap();
    pool.close().await;
}

#[tokio::test]
async fn mssql_skip_mode_reports_lines_and_commits_the_rest() {
    let Some(url) = mssql_url().await else { return };
    let pool = DbPool::open_mssql(&url).await.unwrap();
    pool.execute("IF OBJECT_ID('dbo.import_skip', 'U') IS NOT NULL DROP TABLE dbo.import_skip")
        .await
        .unwrap();
    pool.execute(
        "CREATE TABLE dbo.import_skip (
            id int PRIMARY KEY,
            name nvarchar(100) NOT NULL,
            weight float NULL
        )",
    )
    .await
    .unwrap();
    let tables = pool.introspect().await.unwrap();
    let table = find(&tables, "import_skip");

    let text = "id,name,weight\n1,ada,1.5\n2,grace,heavy\n3,,3.5\n4,alan,4.5\n";
    let report = import(
        &pool,
        table,
        &options(
            vec![bind(0, "id"), bind(1, "name"), bind(2, "weight")],
            ErrorMode::Skip,
        ),
        csv(text).as_mut(),
    )
    .await
    .unwrap();

    assert_eq!(report.inserted_rows, 2);
    assert_eq!(
        report.skipped.iter().map(|s| s.line).collect::<Vec<_>>(),
        vec![3, 4]
    );
    assert_eq!(
        pool.count_table_rows(Reading::UNMARKED, table)
            .await
            .unwrap(),
        2
    );

    pool.execute("DROP TABLE dbo.import_skip").await.unwrap();
    pool.close().await;
}

#[tokio::test]
async fn mssql_import_counts_its_own_rows_not_the_triggers() {
    // Every batch is held to the rows it carried, and SQL Server reports a
    // trigger's DML alongside the statement that fired it. Summed, a table
    // with an audit trigger refused every import: three rows read as six.
    let Some(url) = mssql_url().await else { return };
    let pool = DbPool::open_mssql(&url).await.unwrap();
    for sql in [
        "IF OBJECT_ID('dbo.import_audited', 'U') IS NOT NULL DROP TABLE dbo.import_audited",
        "IF OBJECT_ID('dbo.import_audit', 'U') IS NOT NULL DROP TABLE dbo.import_audit",
        "CREATE TABLE dbo.import_audited (id int PRIMARY KEY, name nvarchar(100) NOT NULL)",
        "CREATE TABLE dbo.import_audit (n int IDENTITY PRIMARY KEY, name nvarchar(100) NOT NULL)",
        "CREATE TRIGGER dbo.import_audited_log ON dbo.import_audited AFTER INSERT AS
         INSERT INTO dbo.import_audit (name) SELECT name FROM inserted",
    ] {
        pool.execute(sql).await.unwrap();
    }
    let tables = pool.introspect().await.unwrap();
    let table = find(&tables, "import_audited");

    let report = import(
        &pool,
        table,
        &options(vec![bind(0, "id"), bind(1, "name")], ErrorMode::Abort),
        csv("id,name\n1,ada\n2,grace\n3,alan\n").as_mut(),
    )
    .await
    .expect("the trigger's rows are not the import's");
    assert_eq!(report.inserted_rows, 3);
    assert_eq!(
        pool.count_table_rows(Reading::UNMARKED, table)
            .await
            .unwrap(),
        3
    );
    // The trigger ran, so its rows were in the response the guard read.
    let audit = pool
        .query_unbounded("SELECT COUNT(*) FROM dbo.import_audit")
        .await
        .unwrap();
    assert_eq!(audit.rows, vec![vec![Value::Integer(3)]]);

    pool.execute("DROP TABLE dbo.import_audited").await.unwrap();
    pool.execute("DROP TABLE dbo.import_audit").await.unwrap();
    pool.close().await;
}

#[tokio::test]
async fn mssql_an_instead_of_insert_trigger_refuses_the_import() {
    // An INSTEAD OF trigger replaces the INSERT, and the count SQL Server
    // reports for the statement is the rows the trigger fired for, not the
    // rows it wrote. Measured: this trigger keeps only even ids, four rows
    // imported, two landed, and the guard saw 4 of 4. No count can catch it,
    // so the table is refused before anything is read from the file.
    let Some(url) = mssql_url().await else { return };
    let pool = DbPool::open_mssql(&url).await.unwrap();
    for sql in [
        "IF OBJECT_ID('dbo.import_instead', 'U') IS NOT NULL DROP TABLE dbo.import_instead",
        "CREATE TABLE dbo.import_instead (id int PRIMARY KEY, name nvarchar(100) NOT NULL)",
        "CREATE TRIGGER dbo.import_instead_evens ON dbo.import_instead INSTEAD OF INSERT AS
         SET NOCOUNT ON;
         INSERT INTO dbo.import_instead SELECT id, name FROM inserted WHERE id % 2 = 0",
    ] {
        pool.execute(sql).await.unwrap();
    }
    let tables = pool.introspect().await.unwrap();
    let table = find(&tables, "import_instead");

    let err = import(
        &pool,
        table,
        &options(vec![bind(0, "id"), bind(1, "name")], ErrorMode::Abort),
        csv("id,name\n1,ada\n2,grace\n3,alan\n4,edsger\n").as_mut(),
    )
    .await
    .expect_err("an import the guard cannot check must be refused");
    assert!(
        err.message.contains(INSTEAD_OF_TRIGGER.message()),
        "{}",
        err.message
    );
    assert_eq!(err.undone_rows, 0);
    assert_eq!(
        pool.count_table_rows(Reading::UNMARKED, table)
            .await
            .unwrap(),
        0
    );

    pool.execute("DROP TABLE dbo.import_instead").await.unwrap();
    pool.close().await;
}

#[tokio::test]
async fn postgres_a_partially_applied_batch_is_rolled_back_and_counted() {
    // The row-count guard's own case, attacked the way the reviewer did: a
    // BEFORE INSERT trigger drops half of every batch, so the statement
    // affects fewer rows than it carried values for. The batch must not
    // commit — and `undone_rows` must count the rows that DID land inside the
    // transaction, since that is the number the safety claim is about.
    let Some(url) = pg_url().await else { return };
    let pool = DbPool::open_postgres(&url).await.unwrap();
    pool.execute("DROP TABLE IF EXISTS import_partial")
        .await
        .unwrap();
    pool.execute("CREATE TABLE import_partial (id integer PRIMARY KEY, name text)")
        .await
        .unwrap();
    pool.execute(
        "CREATE OR REPLACE FUNCTION import_drop_odd() RETURNS trigger AS $$
         BEGIN
             IF NEW.id % 2 = 1 THEN RETURN NULL; END IF;
             RETURN NEW;
         END; $$ LANGUAGE plpgsql",
    )
    .await
    .unwrap();
    pool.execute(
        "CREATE TRIGGER import_drop_odd BEFORE INSERT ON import_partial
         FOR EACH ROW EXECUTE FUNCTION import_drop_odd()",
    )
    .await
    .unwrap();
    let tables = pool.introspect().await.unwrap();
    let table = find(&tables, "import_partial");

    let err = import(
        &pool,
        table,
        &options(vec![bind(0, "id"), bind(1, "name")], ErrorMode::Abort),
        csv("id,name\n1,a\n2,b\n3,c\n4,d\n").as_mut(),
    )
    .await
    .unwrap_err();

    assert!(
        err.message.contains("affected 2 rows, expected 4"),
        "{}",
        err.message
    );
    // The two rows the trigger let through were applied and then undone —
    // reporting 0 here would under-count the one case the guard fires in.
    assert_eq!(err.undone_rows, 2);
    assert_eq!(
        pool.count_table_rows(Reading::UNMARKED, table)
            .await
            .unwrap(),
        0
    );

    pool.execute("DROP TABLE import_partial").await.unwrap();
    pool.execute("DROP FUNCTION import_drop_odd()")
        .await
        .unwrap();
    pool.close().await;
}

#[tokio::test]
async fn postgres_a_range_column_takes_the_literal_the_server_accepts() {
    // `int4range` contains "int", so it used to be read as an integer column
    // and `[1,10)` — which the server takes happily — was silently SKIPPED.
    // A row dropped in skip mode is data loss wearing a success message.
    let Some(url) = pg_url().await else { return };
    let pool = DbPool::open_postgres(&url).await.unwrap();
    pool.execute("DROP TABLE IF EXISTS import_ranges")
        .await
        .unwrap();
    pool.execute(
        "CREATE TABLE import_ranges (id integer PRIMARY KEY, span int4range, days daterange)",
    )
    .await
    .unwrap();
    let tables = pool.introspect().await.unwrap();
    let table = find(&tables, "import_ranges");

    let report = import(
        &pool,
        table,
        &options(
            vec![bind(0, "id"), bind(1, "span"), bind(2, "days")],
            ErrorMode::Skip,
        ),
        csv("id,span,days\n1,\"[1,10)\",\"[2024-01-01,2024-02-01)\"\n").as_mut(),
    )
    .await
    .unwrap();

    assert_eq!(report.inserted_rows, 1, "skipped: {:?}", report.skipped);
    assert_eq!(report.skipped_rows, 0);
    // Read back through the server's own text rendering: hubro's grid shows a
    // range as `<int4range>`, so `SELECT *` would say nothing about the value
    // that landed.
    let stored = pool
        .query_unbounded("SELECT span::text, days::text FROM import_ranges")
        .await
        .unwrap();
    assert_eq!(stored.rows[0][0], Value::Text("[1,10)".into()));
    assert_eq!(
        stored.rows[0][1],
        Value::Text("[2024-01-01,2024-02-01)".into())
    );

    pool.execute("DROP TABLE import_ranges").await.unwrap();
    pool.close().await;
}

#[tokio::test]
async fn mssql_a_bit_column_takes_the_boolean_spellings_people_export() {
    // SQL Server's `bit` IS its boolean type, but the name alone cannot say
    // so (Postgres `bit` is a bit-string). Without the dialect-aware
    // refinement, `yes`/`no` reached the server as text and failed there —
    // and a server-side failure is unskippable, so skip mode could not save
    // the file either.
    let Some(url) = mssql_url().await else { return };
    let pool = DbPool::open_mssql(&url).await.unwrap();
    pool.execute("IF OBJECT_ID('dbo.import_bit', 'U') IS NOT NULL DROP TABLE dbo.import_bit")
        .await
        .unwrap();
    pool.execute("CREATE TABLE dbo.import_bit (id int PRIMARY KEY, active bit NOT NULL)")
        .await
        .unwrap();
    let tables = pool.introspect().await.unwrap();
    let table = find(&tables, "import_bit");
    let mapping = vec![bind(0, "id"), bind(1, "active")];

    let report = import(
        &pool,
        table,
        &options(mapping.clone(), ErrorMode::Abort),
        csv("id,active\n1,yes\n2,no\n3,true\n4,0\n").as_mut(),
    )
    .await
    .unwrap();
    assert_eq!(report.inserted_rows, 4);
    assert_eq!(
        snapshot(&pool, table, "id").await,
        vec![
            vec!["1".to_string(), "1".into()],
            vec!["2".to_string(), "0".into()],
            vec!["3".to_string(), "1".into()],
            vec!["4".to_string(), "0".into()],
        ]
    );

    // And a value that is neither is refused by hubro, so skip mode can
    // actually skip it instead of the server aborting the whole import.
    let report = import(
        &pool,
        table,
        &options(mapping, ErrorMode::Skip),
        csv("id,active\n5,maybe\n6,yes\n").as_mut(),
    )
    .await
    .unwrap();
    assert_eq!(report.inserted_rows, 1);
    assert_eq!(
        report.skipped.iter().map(|s| s.line).collect::<Vec<_>>(),
        vec![2]
    );

    pool.execute("DROP TABLE dbo.import_bit").await.unwrap();
    pool.close().await;
}

/// A file whose empty fields land in NOT NULL columns that declare a
/// default, in a different combination on each line — so on SQLite, which
/// has no `DEFAULT` keyword and leaves such a column out of the statement
/// instead, consecutive rows disagree about which columns to name. Line 6
/// leaves a NOT NULL column with *no* default empty, which is still the
/// error skip mode exists for — and its blank `qty`, being on a skipped row,
/// is not counted as a value that took its default.
const DEFAULTS_CSV: &str = "id,qty,status,note,label\n\
                            1,,,,a\n\
                            2,3,open,x,b\n\
                            3,,open,,c\n\
                            4,5,,y,d\n\
                            5,,done,z,\n\
                            6,,,,f\n";

/// Imports [`DEFAULTS_CSV`] into `import_defaults` (already created by the
/// caller with `qty … NOT NULL DEFAULT 7`, `status … NOT NULL DEFAULT 'new'`,
/// a nullable `note` and a defaultless NOT NULL `label`) and checks that an
/// empty field there became the column's default rather than a NULL the
/// server refuses.
///
/// Before, the NOT NULL check exempted any column with a default but the
/// INSERT still named it and sent `NULL`, which every engine rejects — on a
/// server-side error the whole import aborts in both modes, so a single
/// blank `created_at` threw away the file with no skipped-row line.
async fn assert_empty_fields_take_the_column_default(pool: &DbPool) {
    let tables = pool.introspect().await.unwrap();
    let table = find(&tables, "import_defaults");
    let report = import(
        pool,
        table,
        &options(
            vec![
                bind(0, "id"),
                bind(1, "qty"),
                bind(2, "status"),
                bind(3, "note"),
                bind(4, "label"),
            ],
            ErrorMode::Skip,
        ),
        csv(DEFAULTS_CSV).as_mut(),
    )
    .await
    .unwrap_or_else(|err| panic!("{:?}: {err}", pool.dialect()));

    assert_eq!(report.inserted_rows, 5, "{:?}", pool.dialect());
    // Two on line 2, one each on lines 4 and 5, two on line 7: what the
    // status line reports as values the file did not contain.
    assert_eq!(report.defaulted_cells, 6, "{:?}", pool.dialect());
    assert_eq!(
        report.skipped.iter().map(|s| s.line).collect::<Vec<_>>(),
        vec![6],
        "{:?}: only the defaultless NOT NULL column's blank is refused",
        pool.dialect()
    );
    assert!(
        report.skipped[0].reason.contains("\"label\""),
        "{:?}",
        report.skipped[0]
    );
    let row = |cells: [&str; 5]| cells.map(str::to_string).to_vec();
    assert_eq!(
        snapshot(pool, table, "id").await,
        vec![
            row(["1", "7", "new", "NULL", "a"]),
            row(["2", "3", "open", "x", "b"]),
            row(["3", "7", "open", "NULL", "c"]),
            row(["4", "5", "new", "y", "d"]),
            row(["6", "7", "new", "NULL", "f"]),
        ],
        "{:?}: a blank in a NOT NULL column with a default takes the default; \
         a blank in a nullable one stays NULL",
        pool.dialect()
    );

    // A row whose every mapped column is defaulted: SQLite has nothing left
    // to name, so it is sent as `DEFAULT VALUES` — one row per statement,
    // which two such rows in a row have to respect.
    let tables = pool.introspect().await.unwrap();
    let table = find(&tables, "import_all_default");
    let report = import(
        pool,
        table,
        &options(vec![bind(0, "qty")], ErrorMode::Abort),
        csv("qty\n\"\"\n\"\"\n4\n\"\"\n").as_mut(),
    )
    .await
    .unwrap_or_else(|err| panic!("{:?}: {err}", pool.dialect()));
    assert_eq!(report.inserted_rows, 4, "{:?}", pool.dialect());
    assert_eq!(report.defaulted_cells, 3, "{:?}", pool.dialect());
    let qty: Vec<String> = snapshot(pool, table, "id")
        .await
        .into_iter()
        .map(|row| row[1].clone())
        .collect();
    assert_eq!(qty, vec!["7", "7", "4", "7"], "{:?}", pool.dialect());
}

#[tokio::test]
async fn sqlite_an_empty_field_in_a_not_null_column_takes_its_default() {
    let fixture = FixtureDb::with_sql("").await;
    let pool = fixture.open().await;
    pool.execute(
        "CREATE TABLE import_defaults (
            id INTEGER PRIMARY KEY,
            qty INTEGER NOT NULL DEFAULT 7,
            status TEXT NOT NULL DEFAULT 'new',
            note TEXT,
            label TEXT NOT NULL
        )",
    )
    .await
    .unwrap();
    pool.execute(
        "CREATE TABLE import_all_default (id INTEGER PRIMARY KEY, qty INTEGER NOT NULL DEFAULT 7)",
    )
    .await
    .unwrap();
    assert_empty_fields_take_the_column_default(&pool).await;
}

#[tokio::test]
async fn sqlite_a_batch_split_into_statements_counts_every_row_it_undid() {
    // SQLite sends a batch as several statements when rows disagree about
    // which columns are defaulted. A failure in a later one must still count
    // the rows the earlier ones had landed, or `undone_rows` under-reports
    // exactly what the rollback undid.
    let fixture = FixtureDb::with_sql("").await;
    let pool = fixture.open().await;
    pool.execute("CREATE TABLE split (id INTEGER PRIMARY KEY, qty INTEGER NOT NULL DEFAULT 7)")
        .await
        .unwrap();
    pool.execute("INSERT INTO split VALUES (9, 1)")
        .await
        .unwrap();
    let tables = pool.introspect().await.unwrap();
    let table = find(&tables, "split");
    // Rows 1-2 default `qty` (one statement), row 3 names it and collides
    // with the existing key (the next statement).
    let err = import(
        &pool,
        table,
        &options(vec![bind(0, "id"), bind(1, "qty")], ErrorMode::Abort),
        csv("id,qty\n1,\n2,\n9,3\n").as_mut(),
    )
    .await
    .unwrap_err();
    assert!(err.message.contains("UNIQUE"), "{}", err.message);
    assert_eq!(err.undone_rows, 2);
    assert_eq!(
        pool.count_table_rows(Reading::UNMARKED, table)
            .await
            .unwrap(),
        1
    );
}

#[tokio::test]
async fn sqlite_a_short_statement_in_a_split_batch_counts_every_row_it_undid() {
    // The row-count guard's other arm, inside a split batch: a trigger
    // silently drops one row of the SECOND statement, so it affects fewer
    // rows than it carried. The import must abort, roll back, and count the
    // first statement's rows as well as the second's — `RAISE(IGNORE)`
    // leaves the dropped row out of `changes()`, which is exactly the
    // mismatch the guard exists to catch.
    let fixture = FixtureDb::with_sql("").await;
    let pool = fixture.open().await;
    pool.execute("CREATE TABLE split_ign (id INTEGER PRIMARY KEY, qty INTEGER NOT NULL DEFAULT 7)")
        .await
        .unwrap();
    pool.execute(
        "CREATE TRIGGER split_ign_drop BEFORE INSERT ON split_ign WHEN NEW.id = 4 \
         BEGIN SELECT RAISE(IGNORE); END",
    )
    .await
    .unwrap();
    let tables = pool.introspect().await.unwrap();
    let table = find(&tables, "split_ign");
    // Statement 1: rows 1-2 with `qty` left out (2 land). Statement 2: rows
    // 3-4 naming it, where row 4 is ignored (1 of 2 lands).
    let err = import(
        &pool,
        table,
        &options(vec![bind(0, "id"), bind(1, "qty")], ErrorMode::Abort),
        csv("id,qty\n1,\n2,\n3,5\n4,6\n").as_mut(),
    )
    .await
    .unwrap_err();
    assert!(
        err.message.contains("affected 1 rows, expected 2"),
        "{}",
        err.message
    );
    assert_eq!(err.undone_rows, 3);
    assert_eq!(
        pool.count_table_rows(Reading::UNMARKED, table)
            .await
            .unwrap(),
        0
    );
}

#[tokio::test]
async fn postgres_an_empty_field_in_a_not_null_column_takes_its_default() {
    let Some(url) = pg_url().await else { return };
    let pool = DbPool::open_postgres(&url).await.unwrap();
    pool.execute("DROP TABLE IF EXISTS import_defaults, import_all_default")
        .await
        .unwrap();
    pool.execute(
        "CREATE TABLE import_defaults (
            id integer PRIMARY KEY,
            qty integer NOT NULL DEFAULT 7,
            status text NOT NULL DEFAULT 'new',
            note text,
            label text NOT NULL
        )",
    )
    .await
    .unwrap();
    pool.execute(
        "CREATE TABLE import_all_default (id serial PRIMARY KEY, qty integer NOT NULL DEFAULT 7)",
    )
    .await
    .unwrap();
    assert_empty_fields_take_the_column_default(&pool).await;

    // The issue's own shape: a serial key and a `now()` timestamp, both left
    // empty, plus an identity column — each of which the server fills in.
    pool.execute("DROP TABLE IF EXISTS import_events")
        .await
        .unwrap();
    pool.execute(
        "CREATE TABLE import_events (
            id serial PRIMARY KEY,
            created_at timestamptz NOT NULL DEFAULT now(),
            seq integer GENERATED BY DEFAULT AS IDENTITY,
            note text
        )",
    )
    .await
    .unwrap();
    let tables = pool.introspect().await.unwrap();
    let table = find(&tables, "import_events");
    let report = import(
        &pool,
        table,
        &options(
            vec![
                bind(0, "id"),
                bind(1, "created_at"),
                bind(2, "seq"),
                bind(3, "note"),
            ],
            ErrorMode::Skip,
        ),
        csv("id,created_at,seq,note\n,,,first\n,2024-01-01 00:00:00+00,,second\n").as_mut(),
    )
    .await
    .unwrap();
    assert_eq!(report.inserted_rows, 2);
    assert_eq!(report.skipped_rows, 0);
    // id, created_at and seq on the first row; id and seq on the second.
    assert_eq!(report.defaulted_cells, 5);
    let stored = pool
        .query_unbounded(
            "SELECT id, seq, created_at IS NOT NULL, \
             created_at = '2024-01-01 00:00:00+00' FROM import_events ORDER BY seq",
        )
        .await
        .unwrap();
    let rows: Vec<Vec<String>> = stored
        .rows
        .iter()
        .map(|row| row.iter().map(Value::display).collect())
        .collect();
    // Each row's `seq`, whether `created_at` was filled, and whether it is
    // the file's own timestamp: the first row's was blank, the second's not.
    let rest: Vec<&[String]> = rows.iter().map(|row| &row[1..]).collect();
    assert_eq!(
        rest,
        vec![&["1", "true", "false"][..], &["2", "true", "true"][..]]
    );
    // The serial key the server filled in. On Postgres that is its sequence,
    // so exactly 1 and 2; other engines speaking the protocol fill a serial
    // their own way (CockroachDB's is `unique_rowid()`), where all that can
    // be asserted is two distinct keys.
    let ids: Vec<&str> = rows.iter().map(|row| row[0].as_str()).collect();
    if pool.pg_flavor() == Some(PgFlavor::Postgres) {
        assert_eq!(ids, vec!["1", "2"]);
    } else {
        assert_ne!(ids[0], ids[1], "{ids:?}");
    }

    pool.execute("DROP TABLE import_defaults, import_all_default, import_events")
        .await
        .unwrap();
    pool.close().await;
}

#[tokio::test]
async fn mssql_an_empty_field_in_a_not_null_column_takes_its_default() {
    let Some(url) = mssql_url().await else { return };
    let pool = DbPool::open_mssql(&url).await.unwrap();
    pool.execute(
        "IF OBJECT_ID('dbo.import_defaults', 'U') IS NOT NULL DROP TABLE dbo.import_defaults",
    )
    .await
    .unwrap();
    pool.execute(
        "IF OBJECT_ID('dbo.import_all_default', 'U') IS NOT NULL DROP TABLE dbo.import_all_default",
    )
    .await
    .unwrap();
    pool.execute(
        "CREATE TABLE dbo.import_defaults (
            id int PRIMARY KEY,
            qty int NOT NULL DEFAULT 7,
            status nvarchar(20) NOT NULL DEFAULT N'new',
            note nvarchar(20) NULL,
            label nvarchar(20) NOT NULL
        )",
    )
    .await
    .unwrap();
    pool.execute(
        "CREATE TABLE dbo.import_all_default (
            id int IDENTITY PRIMARY KEY,
            qty int NOT NULL DEFAULT 7
        )",
    )
    .await
    .unwrap();
    assert_empty_fields_take_the_column_default(&pool).await;

    pool.execute("DROP TABLE dbo.import_defaults, dbo.import_all_default")
        .await
        .unwrap();
    pool.close().await;
}

#[tokio::test]
async fn mssql_import_a_committing_trigger_kept_is_not_reported_undone() {
    // Measured: an AFTER trigger that commits fails its batch with 3609, and
    // every row sent before it stays in the table.
    let Some(url) = mssql_url().await else { return };
    let pool = DbPool::open_mssql(&url).await.unwrap();
    pool.execute("DROP TABLE IF EXISTS dbo.import_trigger_commit")
        .await
        .unwrap();
    pool.execute("CREATE TABLE dbo.import_trigger_commit (id int PRIMARY KEY, name nvarchar(100))")
        .await
        .unwrap();
    let statements = split_statements(
        "CREATE TRIGGER dbo.import_trigger_commit_t ON dbo.import_trigger_commit AFTER INSERT AS \
         IF EXISTS (SELECT 1 FROM inserted WHERE id = 1100) COMMIT",
        pool.dialect(),
    );
    run_script(&pool, pool.backend_capabilities(), &statements, |_| {})
        .await
        .unwrap();
    let tables = pool.introspect().await.unwrap();
    let table = find(&tables, "import_trigger_commit");
    let mut file = String::from("id,name\n");
    for i in 1..=1200 {
        file.push_str(&format!("{i},row{i}\n"));
    }
    let err = import(
        &pool,
        table,
        &options(vec![bind(0, "id"), bind(1, "name")], ErrorMode::Abort),
        csv(file.as_str()).as_mut(),
    )
    .await
    .unwrap_err();
    let kept = pool
        .count_table_rows(Reading::UNMARKED, table)
        .await
        .unwrap();
    assert_eq!(
        kept, 1200,
        "the trigger's COMMIT kept every row sent: {err}"
    );
    assert_eq!(err.undone_rows, 0, "{err}");
    assert!(err.message.contains("may have been kept"), "{err}");
    pool.execute("DROP TABLE dbo.import_trigger_commit")
        .await
        .unwrap();
    pool.close().await;
}

#[tokio::test]
async fn sqlite_a_blank_rowid_alias_takes_the_next_rowid_and_a_blank_without_rowid_key_is_skipped()
{
    // SQLite fills a NULL rowid alias with the next rowid and never applies
    // its NOT NULL. A WITHOUT ROWID table's INTEGER key has no rowid behind
    // it, so a blank there really is a NOT NULL violation, and skip mode must
    // be able to skip it rather than fail at the server.
    let fixture = FixtureDb::with_sql("").await;
    let pool = fixture.open().await;
    pool.execute("CREATE TABLE alias (id INTEGER PRIMARY KEY NOT NULL, name TEXT NOT NULL)")
        .await
        .unwrap();
    pool.execute(
        "CREATE TABLE keyed (id INTEGER PRIMARY KEY NOT NULL, name TEXT NOT NULL) WITHOUT ROWID",
    )
    .await
    .unwrap();
    let tables = pool.introspect().await.unwrap();
    let text = "id,name\n,ada\n5,grace\n,alan\n";
    let mapping = vec![bind(0, "id"), bind(1, "name")];

    let alias = find(&tables, "alias");
    let report = import(
        &pool,
        alias,
        &options(mapping.clone(), ErrorMode::Skip),
        csv(text).as_mut(),
    )
    .await
    .unwrap();
    assert_eq!(report.inserted_rows, 3, "{report:?}");
    assert_eq!(report.skipped_rows, 0, "{report:?}");
    assert_eq!(report.defaulted_cells, 2, "{report:?}");
    assert_eq!(
        snapshot(&pool, alias, "id").await,
        vec![
            vec!["1".to_string(), "ada".into()],
            vec!["5".to_string(), "grace".into()],
            vec!["6".to_string(), "alan".into()],
        ]
    );

    let keyed = find(&tables, "keyed");
    let report = import(
        &pool,
        keyed,
        &options(mapping, ErrorMode::Skip),
        csv(text).as_mut(),
    )
    .await
    .unwrap();
    assert_eq!(report.inserted_rows, 1, "{report:?}");
    assert_eq!(
        report.skipped.iter().map(|s| s.line).collect::<Vec<_>>(),
        vec![2, 4]
    );
    assert!(
        report.skipped[0]
            .reason
            .contains("NOT NULL and has no default"),
        "{:?}",
        report.skipped[0]
    );
    assert_eq!(
        snapshot(&pool, keyed, "id").await,
        vec![vec!["5".to_string(), "grace".into()]]
    );
}
