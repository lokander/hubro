//! Round-trip tests for the clipboard's `INSERT` generation (FRE-110).
//!
//! The unit tests in `db::clipboard` pin the exact text of every format. These
//! go one step further for the format that can corrupt data silently: they
//! render `INSERT`s from real rows, execute them **unedited** against the very
//! engine they were rendered for, and assert the copied rows come back
//! byte-identical to the originals.
//!
//! That is the only way to catch the subtle ones — a NULL pasted back as `''`,
//! a `char(0x00ff)` blob mangled by the wrong binary literal, or non-Latin
//! text silently turned into `?` by an unprefixed T-SQL string literal — all
//! of which are perfectly valid SQL that runs without complaint.
//!
//! SQLite always runs. Postgres is skipped unless `HUBRO_PG_TEST_URL` is set,
//! SQL Server unless `HUBRO_MSSQL_TEST_URL` is (see `db_postgres.rs` /
//! `db_sqlserver.rs` for the Docker commands).

mod common;

use common::FixtureDb;
use hubro::db::{
    render_copy, split_statements, CopyBlock, CopyFormat, DbPool, Dialect, QueryResult, Value,
};

/// The columns every fixture below carries, in order.
const COLUMNS: [&str; 5] = ["id", "name", "note", "weight", "data"];

/// The values that make this test worth running. Every one of them is a way
/// the generated SQL could be wrong while still executing cleanly.
fn columns() -> Vec<String> {
    COLUMNS.iter().map(|c| (*c).to_string()).collect()
}

/// Reads back the rows of `table` in id order.
async fn read_rows(pool: &DbPool, table: &str) -> Vec<Vec<Value>> {
    let sql = format!(
        "SELECT \"id\", \"name\", \"note\", \"weight\", \"data\" FROM \"{table}\" ORDER BY \"id\""
    );
    let QueryResult { rows, .. } = pool.query(&sql).await.expect("read back the copied rows");
    rows
}

/// Renders the clipboard's INSERT statements for `rows` targeting `table`, and
/// runs them one at a time. The script is split with the app's own splitter
/// because a value can contain a literal `;` or newline — naive line splitting
/// would tear a statement in half and hide exactly the bug this is hunting.
async fn paste_inserts(pool: &DbPool, dialect: Dialect, table: &str, rows: &[Vec<Value>]) {
    let block = CopyBlock {
        schema: None,
        table: table.to_string(),
        columns: columns(),
        rows: rows.to_vec(),
    };
    let script = render_copy(&block, CopyFormat::Insert, Some(dialect))
        .expect("INSERT renders whenever a dialect is given");
    let statements = split_statements(&script, dialect);
    assert_eq!(
        statements.len(),
        rows.len(),
        "one INSERT per row; got:\n{script}"
    );
    for statement in statements {
        pool.execute(&statement)
            .await
            .unwrap_or_else(|err| panic!("generated INSERT failed: {err}\n{statement}"));
    }
}

#[tokio::test]
async fn sqlite_generated_inserts_round_trip() {
    // `note` holds the pair that must never collapse: a NULL and an empty
    // string. `data` holds a blob with a NUL and a 0xff byte plus an empty
    // blob. `name` holds quotes, a backslash, a newline, a semicolon, a tab
    // and non-ASCII text.
    let fixture = FixtureDb::with_sql(
        r#"
        CREATE TABLE src (id INTEGER PRIMARY KEY, name TEXT, note TEXT, weight REAL, data BLOB);
        CREATE TABLE dst (id INTEGER PRIMARY KEY, name TEXT, note TEXT, weight REAL, data BLOB);
        INSERT INTO src VALUES (1, 'O''Brien''s ''Reel''', NULL, 1.5, x'00ff10');
        INSERT INTO src VALUES (2, 'back\slash; and -- comment', '', -0.25, x'');
        INSERT INTO src VALUES (3, 'two' || char(10) || 'lines' || char(9) || 'tabbed', 'plain', 0.0, NULL);
        INSERT INTO src VALUES (4, 'héllo 世界 🦀', 'NULL', 1e300, x'de');
        INSERT INTO src VALUES (5, NULL, NULL, NULL, NULL);
        "#,
    )
    .await;
    let pool = fixture.open().await;

    let source = read_rows(&pool, "src").await;
    assert_eq!(source.len(), 5);
    // The fixture really does hold a NULL and an empty string in `note`.
    assert_eq!(source[0][2], Value::Null);
    assert_eq!(source[1][2], Value::Text(String::new()));

    paste_inserts(&pool, Dialect::Sqlite, "dst", &source).await;
    assert_eq!(read_rows(&pool, "dst").await, source);
}

#[tokio::test]
async fn postgres_generated_inserts_round_trip() {
    let Ok(url) = std::env::var("HUBRO_PG_TEST_URL") else {
        eprintln!("skipping postgres clipboard test: HUBRO_PG_TEST_URL not set");
        return;
    };
    let pool = DbPool::open_postgres(&url).await.unwrap();
    for table in ["clip_src", "clip_dst"] {
        pool.execute(&format!("DROP TABLE IF EXISTS {table}"))
            .await
            .unwrap();
        pool.execute(&format!(
            "CREATE TABLE {table} (
                id integer PRIMARY KEY,
                name text,
                note text,
                weight double precision,
                data bytea
            )"
        ))
        .await
        .unwrap();
    }
    pool.execute(
        r#"INSERT INTO clip_src VALUES
            (1, 'O''Brien''s ''Reel''', NULL, 1.5, '\x00ff10'),
            (2, E'back\\slash; and -- comment', '', -0.25, '\x'),
            (3, E'two\nlines\ttabbed', 'plain', 0.0, NULL),
            (4, 'héllo 世界 🦀', 'NULL', 1e300, '\xde'),
            (5, NULL, NULL, NULL, NULL)"#,
    )
    .await
    .unwrap();

    let source = read_rows(&pool, "clip_src").await;
    assert_eq!(source.len(), 5);
    assert_eq!(source[0][2], Value::Null);
    assert_eq!(source[1][2], Value::Text(String::new()));
    // A backslash must survive as one character: the generated literal relies
    // on `standard_conforming_strings`, so a doubled backslash here would mean
    // the rendering is wrong for every escape.
    assert_eq!(
        source[1][1],
        Value::Text("back\\slash; and -- comment".into())
    );

    paste_inserts(&pool, Dialect::Postgres, "clip_dst", &source).await;
    assert_eq!(read_rows(&pool, "clip_dst").await, source);

    for table in ["clip_src", "clip_dst"] {
        pool.execute(&format!("DROP TABLE {table}")).await.unwrap();
    }
}

#[tokio::test]
async fn postgres_generated_inserts_carry_non_finite_floats() {
    // Postgres is the one backend whose floats can hold NaN/±Infinity, and
    // they have no numeric literal form — the copy has to spell them as
    // quoted casts or the paste either fails or lands as NULL.
    let Ok(url) = std::env::var("HUBRO_PG_TEST_URL") else {
        eprintln!("skipping postgres clipboard test: HUBRO_PG_TEST_URL not set");
        return;
    };
    let pool = DbPool::open_postgres(&url).await.unwrap();
    pool.execute("DROP TABLE IF EXISTS clip_floats")
        .await
        .unwrap();
    pool.execute("CREATE TABLE clip_floats (id integer PRIMARY KEY, x double precision)")
        .await
        .unwrap();

    let rows = vec![
        vec![Value::Integer(1), Value::Real(f64::NAN)],
        vec![Value::Integer(2), Value::Real(f64::INFINITY)],
        vec![Value::Integer(3), Value::Real(f64::NEG_INFINITY)],
    ];
    let block = CopyBlock {
        schema: None,
        table: "clip_floats".into(),
        columns: vec!["id".into(), "x".into()],
        rows,
    };
    let script = render_copy(&block, CopyFormat::Insert, Some(Dialect::Postgres))
        .expect("INSERT renders whenever a dialect is given");
    for statement in split_statements(&script, Dialect::Postgres) {
        pool.execute(&statement)
            .await
            .unwrap_or_else(|err| panic!("generated INSERT failed: {err}\n{statement}"));
    }

    let back = pool
        .query("SELECT x FROM clip_floats ORDER BY id")
        .await
        .unwrap();
    let floats: Vec<f64> = back
        .rows
        .iter()
        .map(|row| match row[0] {
            Value::Real(x) => x,
            ref other => panic!("expected a float, got {other:?}"),
        })
        .collect();
    assert!(floats[0].is_nan(), "NaN survived as {}", floats[0]);
    assert_eq!(floats[1], f64::INFINITY);
    assert_eq!(floats[2], f64::NEG_INFINITY);

    pool.execute("DROP TABLE clip_floats").await.unwrap();
}

#[tokio::test]
async fn sqlserver_generated_inserts_round_trip() {
    let Ok(url) = std::env::var("HUBRO_MSSQL_TEST_URL") else {
        eprintln!("skipping sql server clipboard test: HUBRO_MSSQL_TEST_URL not set");
        return;
    };
    let pool = DbPool::open_mssql(&url).await.unwrap();
    for table in ["clip_src", "clip_dst"] {
        pool.execute(&format!("DROP TABLE IF EXISTS dbo.{table}"))
            .await
            .unwrap();
        pool.execute(&format!(
            "CREATE TABLE dbo.{table} (
                id int PRIMARY KEY,
                name nvarchar(max),
                note nvarchar(max),
                weight float,
                data varbinary(max)
            )"
        ))
        .await
        .unwrap();
    }
    // N-prefixed here so the *source* really holds the non-ASCII text; the
    // point of the test is whether the generated copy keeps it.
    pool.execute(
        "INSERT INTO dbo.clip_src VALUES
            (1, N'O''Brien''s ''Reel''', NULL, 1.5, 0x00ff10),
            (2, N'back\\slash; and -- comment', N'', -0.25, 0x),
            (3, CONCAT(N'two', CHAR(10), N'lines', CHAR(9), N'tabbed'), N'plain', 0.0, NULL),
            (4, N'héllo 世界', N'NULL', 1e300, 0xde),
            (5, NULL, NULL, NULL, NULL)",
    )
    .await
    .unwrap();

    let source = read_rows(&pool, "clip_src").await;
    assert_eq!(source.len(), 5);
    assert_eq!(source[0][2], Value::Null);
    assert_eq!(source[1][2], Value::Text(String::new()));
    assert_eq!(source[3][1], Value::Text("héllo 世界".into()));

    paste_inserts(&pool, Dialect::SqlServer, "clip_dst", &source).await;
    // Without the `N` prefix on the generated literals, row 4's name comes
    // back as "h?llo ??" here — valid SQL, silently wrong data.
    assert_eq!(read_rows(&pool, "clip_dst").await, source);

    for table in ["clip_src", "clip_dst"] {
        pool.execute(&format!("DROP TABLE dbo.{table}"))
            .await
            .unwrap();
    }
}
