//! End-to-end tests for CSV/JSON import (FRE-112): `run_import` streams a
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

use std::io::BufReader;

use common::FixtureDb;
use hubro::db::{
    run_import, ColumnBinding, CsvDialect, CsvReader, DbPool, Dialect, EmptyField, Encoding,
    ErrorMode, ImportError, ImportOptions, ImportReport, JsonReader, JsonShape, PageRequest,
    RecordSource, SourceField, TableAccess, TableMeta, Value,
};

fn pg_url() -> Option<String> {
    match std::env::var("HUBRO_PG_TEST_URL") {
        Ok(url) => Some(url),
        Err(_) => {
            eprintln!("skipping postgres import test: HUBRO_PG_TEST_URL not set");
            None
        }
    }
}

fn mssql_url() -> Option<String> {
    match std::env::var("HUBRO_MSSQL_TEST_URL") {
        Ok(url) => Some(url),
        Err(_) => {
            eprintln!("skipping sql server import test: HUBRO_MSSQL_TEST_URL not set");
            None
        }
    }
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
    let result = pool.query(&sql).await.unwrap();
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
    let before = snapshot(&pool, people, "id").await;

    // 1200 rows against a 300-row batch (900 parameter ceiling / 3 columns):
    // the bad row is in the last batch, so three batches have already been
    // sent and committed-to-the-transaction when it fails.
    let text = csv_with_a_bad_row(1200, 1150);
    let err = import(
        &pool,
        people,
        &options(
            vec![bind(0, "id"), bind(1, "name"), bind(2, "weight")],
            ErrorMode::Abort,
        ),
        csv(text.as_str()).as_mut(),
    )
    .await
    .unwrap_err();

    // The failure names the line and the column, and reports what it undid.
    assert_eq!(err.line, Some(1151), "the header is line 1");
    assert!(err.message.contains("weight"), "{}", err.message);
    assert!(err.message.contains("not-a-number"), "{}", err.message);
    assert!(
        err.undone_rows >= 900,
        "the import must actually have inserted rows before undoing them, got {}",
        err.undone_rows
    );

    // ...and the table is exactly what it was.
    assert_eq!(snapshot(&pool, people, "id").await, before);
    assert_eq!(pool.count_table_rows(people).await.unwrap(), 1);
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
    assert_eq!(pool.count_table_rows(people).await.unwrap(), 0);
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
        schema: None,
        table: "artists".to_string(),
        limit: 0,
        offset: 0,
        sort: None,
        filter: None,
        extra_key_column: None,
    };
    let (sql, params) = request.export_sql(Dialect::Sqlite);
    let mut buffer: Vec<u8> = Vec::new();
    let exported = pool
        .export(&sql, &params, hubro::db::ExportFormat::Csv, &mut buffer)
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
    let Some(url) = pg_url() else { return };
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
    let Some(url) = pg_url() else { return };
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
    let before = snapshot(&pool, table, "id").await;

    // 60000 parameters / 3 columns is capped at 500 rows per batch, so 1200
    // rows is three batches: the failure has two full batches behind it.
    let text = csv_with_a_bad_row(1200, 1150);
    let err = import(
        &pool,
        table,
        &options(
            vec![bind(0, "id"), bind(1, "name"), bind(2, "weight")],
            ErrorMode::Abort,
        ),
        csv(text.as_str()).as_mut(),
    )
    .await
    .unwrap_err();

    assert_eq!(err.line, Some(1151));
    assert!(
        err.undone_rows >= 1000,
        "rows must really have been inserted before the rollback, got {}",
        err.undone_rows
    );
    assert_eq!(snapshot(&pool, table, "id").await, before);
    assert_eq!(pool.count_table_rows(table).await.unwrap(), 1);

    pool.execute("DROP TABLE import_rollback").await.unwrap();
    pool.close().await;
}

#[tokio::test]
async fn postgres_a_server_side_failure_also_rolls_the_whole_import_back() {
    // The other half of the guarantee: a row hubro accepts but the *server*
    // refuses (a duplicate key) is not skippable in either mode, and must
    // leave nothing behind.
    let Some(url) = pg_url() else { return };
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
            pool.count_table_rows(table).await.unwrap(),
            1,
            "{mode:?}: nothing may survive a server-side rejection"
        );
    }

    pool.execute("DROP TABLE import_conflict").await.unwrap();
    pool.close().await;
}

#[tokio::test]
async fn postgres_skip_mode_reports_lines_and_commits_the_rest() {
    let Some(url) = pg_url() else { return };
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
    assert_eq!(pool.count_table_rows(table).await.unwrap(), 2);

    pool.execute("DROP TABLE import_skip").await.unwrap();
    pool.close().await;
}

#[tokio::test]
async fn mssql_csv_import_lands_rows_and_rolls_a_bad_file_back() {
    let Some(url) = mssql_url() else { return };
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
    assert_eq!(pool.count_table_rows(table).await.unwrap(), 1201);

    // ...then a bad one, which must leave those 1201 rows exactly as they are.
    let before = snapshot(&pool, table, "id").await;
    let mut bad = String::from("id,name,weight,active\n");
    for i in 2001..=3200 {
        if i == 3150 {
            bad.push_str(&format!("{i},row{i},not-a-number,1\n"));
        } else {
            bad.push_str(&format!("{i},row{i},{i}.5,1\n"));
        }
    }
    let err = import(
        &pool,
        table,
        &options(mapping, ErrorMode::Abort),
        csv(bad.as_str()).as_mut(),
    )
    .await
    .unwrap_err();
    assert_eq!(err.line, Some(1151));
    assert!(
        err.undone_rows >= 1000,
        "rows must really have been inserted before the rollback, got {}",
        err.undone_rows
    );
    assert_eq!(snapshot(&pool, table, "id").await, before);
    assert_eq!(pool.count_table_rows(table).await.unwrap(), 1201);

    pool.execute("DROP TABLE dbo.import_rows").await.unwrap();
    pool.close().await;
}

#[tokio::test]
async fn mssql_skip_mode_reports_lines_and_commits_the_rest() {
    let Some(url) = mssql_url() else { return };
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
    assert_eq!(pool.count_table_rows(table).await.unwrap(), 2);

    pool.execute("DROP TABLE dbo.import_skip").await.unwrap();
    pool.close().await;
}

#[tokio::test]
async fn postgres_a_partially_applied_batch_is_rolled_back_and_counted() {
    // The row-count guard's own case, attacked the way the reviewer did: a
    // BEFORE INSERT trigger drops half of every batch, so the statement
    // affects fewer rows than it carried values for. The batch must not
    // commit — and `undone_rows` must count the rows that DID land inside the
    // transaction, since that is the number the safety claim is about.
    let Some(url) = pg_url() else { return };
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
    assert_eq!(pool.count_table_rows(table).await.unwrap(), 0);

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
    let Some(url) = pg_url() else { return };
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
        .query("SELECT span::text, days::text FROM import_ranges")
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
    let Some(url) = mssql_url() else { return };
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
