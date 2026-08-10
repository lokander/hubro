//! SQL Server integration tests. They need a running server (Docker only, per
//! CLAUDE.md) and are skipped unless `HUBRO_MSSQL_TEST_URL` is set, e.g.:
//!
//! ```sh
//! docker run -d --name hubro-mssql-test -e ACCEPT_EULA=Y \
//!   -e 'MSSQL_SA_PASSWORD=Str0ng!Passw0rd' -p 14333:1433 \
//!   mcr.microsoft.com/mssql/server:2022-latest
//! HUBRO_MSSQL_TEST_URL='mssql://sa:Str0ng!Passw0rd@localhost:14333/master?encrypt=on&trustServerCertificate=true' \
//!   cargo test --test db_sqlserver
//! ```
//!
//! The stock container's certificate is self-signed, hence
//! `trustServerCertificate=true` (the form's dev checkbox equivalent).
//!
//! Every test creates and drops its own uniquely-named objects in `dbo`, so
//! the suite is re-runnable and tests stay independent of each other.

use hubro::db::{
    apply_staged, detect_row_identity, mssql_url_with_password, run_script, split_statements,
    Capabilities, DbError, DbPool, Dialect, ExportFormat, Filter, Generated, PageRequest,
    Restriction, Rollback, RowCount, RowIdentity, RowLocator, SortDir, StagedChange,
    StatementOutcome, TableKind, TableMeta, Value, PREVIEW_BYTES, QUERY_CELL_CAP,
};

fn test_url() -> Option<String> {
    match std::env::var("HUBRO_MSSQL_TEST_URL") {
        Ok(url) => Some(url),
        Err(_) => {
            eprintln!("skipping sql server test: HUBRO_MSSQL_TEST_URL not set");
            None
        }
    }
}

/// Whether an error is SQL Server picking this session as a deadlock victim
/// (error 1205). Under cargo's parallel test execution, sibling tests' DDL and
/// catalog reads occasionally deadlock; the server explicitly says "rerun the
/// transaction", so [`run_all`] and [`introspect_table`] retry these a few
/// times. Detection is on the 1205 message text and nothing else, so any real
/// failure still fails fast.
fn is_deadlock_victim(err: &DbError) -> bool {
    err.to_string().contains("deadlock victim")
}

/// Bounded retry for the transient deadlocks above: up to three re-attempts
/// with a short growing backoff.
async fn retry_deadlocks<T, F, Fut>(mut op: F) -> Result<T, DbError>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T, DbError>>,
{
    let mut attempt = 0u32;
    loop {
        match op().await {
            Err(e) if is_deadlock_victim(&e) && attempt < 3 => {
                attempt += 1;
                tokio::time::sleep(std::time::Duration::from_millis(50 * u64::from(attempt))).await;
            }
            other => return other,
        }
    }
}

/// Runs each statement on its own (CREATE VIEW must be alone in its batch, so
/// fixtures are built statement by statement, like the Postgres suite does),
/// retrying deadlock-victim kills. Fixture DDL and drops route through here.
async fn run_all(pool: &DbPool, statements: &[&str]) {
    for sql in statements {
        retry_deadlocks(|| pool.query(sql))
            .await
            .unwrap_or_else(|e| panic!("{sql}: {e}"));
    }
}

/// The introspected metadata for one `dbo` table, retrying deadlock-victim
/// kills (the catalog joins can deadlock with sibling tests' DDL).
async fn introspect_table(pool: &DbPool, name: &str) -> TableMeta {
    let tables = retry_deadlocks(|| pool.introspect()).await.unwrap();
    tables
        .iter()
        .find(|t| t.schema.as_deref() == Some("dbo") && t.name == name)
        .unwrap_or_else(|| panic!("no introspected table dbo.{name}"))
        .clone()
}

fn page_request(table: &str) -> PageRequest {
    PageRequest {
        schema: Some("dbo".into()),
        table: table.into(),
        limit: 100,
        offset: 0,
        sort: None,
        filter: None,
        extra_key_column: None,
    }
}

#[tokio::test]
async fn sqlserver_connects_and_validates_with_a_round_trip() {
    let Some(url) = test_url() else { return };
    // open_mssql itself validates with a SELECT 1 round-trip.
    let pool = DbPool::open_mssql(&url).await.unwrap();
    assert_eq!(pool.dialect(), Dialect::SqlServer);
    let result = pool.query("SELECT 1").await.unwrap();
    assert_eq!(result.rows, vec![vec![Value::Integer(1)]]);
    pool.close().await;
}

#[tokio::test]
async fn sqlserver_bad_password_is_an_authentication_error() {
    let Some(url) = test_url() else { return };
    let wrong = mssql_url_with_password(&url, "definitely-wrong-password").unwrap();
    let err = DbPool::open_mssql(&wrong)
        .await
        .err()
        .expect("wrong password must fail");
    match err {
        DbError::Connect(msg) => assert!(
            msg.contains("authentication failed"),
            "unexpected message: {msg}"
        ),
        other => panic!("expected Connect error, got {other:?}"),
    }
}

#[tokio::test]
async fn sqlserver_introspection_covers_identity_computed_rowversion_and_defaults() {
    let Some(url) = test_url() else { return };
    let pool = DbPool::open_mssql(&url).await.unwrap();
    run_all(
        &pool,
        &[
            "DROP TABLE IF EXISTS dbo.intro_widgets",
            "CREATE TABLE dbo.intro_widgets (
                id int IDENTITY(1,1) NOT NULL PRIMARY KEY,
                name nvarchar(50) NOT NULL,
                price decimal(10,2) NOT NULL DEFAULT 0,
                doubled AS (price * 2),
                stamp rowversion,
                note varchar(max) NULL DEFAULT 'none',
                created datetime2 NOT NULL DEFAULT sysdatetime()
            )",
        ],
    )
    .await;

    let widgets = introspect_table(&pool, "intro_widgets").await;
    assert_eq!(widgets.kind, TableKind::Table);
    let names: Vec<&str> = widgets.columns.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(
        names,
        ["id", "name", "price", "doubled", "stamp", "note", "created"]
    );
    let col = |name: &str| widgets.columns.iter().find(|c| c.name == name).unwrap();

    // IDENTITY: primary key, readable type, and read-only in the editor —
    // an INSERT supplying a value or any UPDATE of it fails server-side, so
    // introspection maps it to Generated::Always.
    let id = col("id");
    assert_eq!(id.type_name, "int");
    assert_eq!(id.primary_key_position, Some(1));
    assert!(!id.nullable);
    assert_eq!(id.generated, Generated::Always);

    let name = col("name");
    assert_eq!(name.type_name, "nvarchar(50)");
    assert!(!name.nullable);
    assert_eq!(name.generated, Generated::Never);

    // Defaults come back unwrapped from SQL Server's parenthesis armor.
    assert_eq!(col("price").type_name, "decimal(10,2)");
    assert_eq!(col("price").default.as_deref(), Some("0"));
    assert_eq!(col("note").type_name, "varchar(max)");
    assert!(col("note").nullable);
    assert_eq!(col("note").default.as_deref(), Some("'none'"));
    assert_eq!(col("created").default.as_deref(), Some("sysdatetime()"));

    // Computed and rowversion columns are database-assigned.
    assert_eq!(col("doubled").generated, Generated::Always);
    let stamp = col("stamp");
    assert_eq!(stamp.type_name, "timestamp"); // sys.types' name for rowversion
    assert_eq!(stamp.generated, Generated::Always);

    assert_eq!(
        detect_row_identity(&widgets, Dialect::SqlServer),
        Some(RowIdentity::PrimaryKey {
            columns: vec!["id".into()]
        })
    );

    run_all(&pool, &["DROP TABLE dbo.intro_widgets"]).await;
    pool.close().await;
}

#[tokio::test]
async fn sqlserver_introspection_covers_indexes_fks_and_views() {
    let Some(url) = test_url() else { return };
    let pool = DbPool::open_mssql(&url).await.unwrap();
    run_all(
        &pool,
        &[
            "DROP VIEW IF EXISTS dbo.intro_child_codes",
            "DROP TABLE IF EXISTS dbo.intro_children",
            "DROP TABLE IF EXISTS dbo.intro_parents",
            "DROP TABLE IF EXISTS dbo.intro_filtered_only",
            "CREATE TABLE dbo.intro_parents (
                region nvarchar(10) NOT NULL,
                slot int NOT NULL,
                label nvarchar(50) NULL,
                CONSTRAINT pk_intro_parents PRIMARY KEY (region, slot)
            )",
            "CREATE TABLE dbo.intro_children (
                id int IDENTITY(1,1) NOT NULL CONSTRAINT pk_intro_children PRIMARY KEY,
                region nvarchar(10) NOT NULL,
                slot int NOT NULL,
                code nvarchar(20) NOT NULL,
                nickname nvarchar(20) NULL,
                CONSTRAINT fk_intro_children_parent FOREIGN KEY (region, slot)
                    REFERENCES dbo.intro_parents (region, slot)
            )",
            "CREATE UNIQUE INDEX ux_intro_children_code ON dbo.intro_children (code)",
            "CREATE UNIQUE INDEX ux_intro_children_nickname ON dbo.intro_children (nickname) \
             WHERE nickname IS NOT NULL",
            // CREATE VIEW must lead its own batch, which the driver's RPC
            // path can't provide — EXEC() gives it one.
            "EXEC('CREATE VIEW dbo.intro_child_codes AS \
             SELECT code FROM dbo.intro_children')",
            "CREATE TABLE dbo.intro_filtered_only (code nvarchar(20) NOT NULL)",
            "CREATE UNIQUE INDEX ux_intro_filtered_only ON dbo.intro_filtered_only (code) \
             WHERE code <> N''",
        ],
    )
    .await;

    // Composite PK in declaration order.
    let parents = introspect_table(&pool, "intro_parents").await;
    let pk: Vec<&str> = parents
        .primary_key()
        .iter()
        .map(|c| c.name.as_str())
        .collect();
    assert_eq!(pk, ["region", "slot"]);

    // Unique vs filtered-unique indexes: a filtered index maps to `partial`
    // and must never serve as a row identity.
    let children = introspect_table(&pool, "intro_children").await;
    assert!(children
        .indexes
        .iter()
        .any(|i| i.name == "ux_intro_children_code"
            && i.unique
            && !i.partial
            && i.columns == ["code"]));
    assert!(children
        .indexes
        .iter()
        .any(|i| i.name == "ux_intro_children_nickname" && i.unique && i.partial));

    // Multi-column FK with ordering, referenced schema, and explicit
    // referenced columns (SQL Server always records them).
    let fk = children
        .foreign_keys
        .iter()
        .find(|fk| fk.referenced_table == "intro_parents")
        .unwrap();
    assert_eq!(fk.columns, ["region", "slot"]);
    assert_eq!(fk.referenced_schema.as_deref(), Some("dbo"));
    assert_eq!(
        fk.referenced_columns,
        [Some("region".to_string()), Some("slot".to_string())]
    );

    // The PK wins as row identity even with a unique index present.
    assert_eq!(
        detect_row_identity(&children, Dialect::SqlServer),
        Some(RowIdentity::PrimaryKey {
            columns: vec!["id".into()]
        })
    );

    // Views introspect with their own kind and columns, and are read-only.
    let view = introspect_table(&pool, "intro_child_codes").await;
    assert_eq!(view.kind, TableKind::View);
    let view_columns: Vec<&str> = view.columns.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(view_columns, ["code"]);
    assert!(detect_row_identity(&view, Dialect::SqlServer).is_none());

    // A table whose only unique index is filtered has no safe row identity.
    let filtered_only = introspect_table(&pool, "intro_filtered_only").await;
    assert!(
        detect_row_identity(&filtered_only, Dialect::SqlServer).is_none(),
        "a filtered unique index must not address rows"
    );

    // Capabilities (FRE-87): full at the connection level, resolved per
    // object, with each narrowing carrying its own reason.
    assert_eq!(pool.backend_capabilities(), Capabilities::FULL);
    let children_access = pool.backend_access(&children);
    assert_eq!(children_access.caps, Capabilities::FULL);
    assert_eq!(children_access.restriction, None);

    let view_access = pool.backend_access(&view);
    assert!(!view_access.can_mutate());
    assert_eq!(view_access.restriction, Some(Restriction::View));
    // Reduced, not disabled — a view still reads and pages.
    assert!(view_access.caps.read_query);
    assert!(view_access.caps.offset_paging);

    let filtered_access = pool.backend_access(&filtered_only);
    assert!(!filtered_access.can_mutate());
    assert_eq!(
        filtered_access.restriction,
        Some(Restriction::NoRowIdentity)
    );

    run_all(
        &pool,
        &[
            "DROP VIEW dbo.intro_child_codes",
            "DROP TABLE dbo.intro_children",
            "DROP TABLE dbo.intro_parents",
            "DROP TABLE dbo.intro_filtered_only",
        ],
    )
    .await;
    pool.close().await;
}

#[tokio::test]
async fn sqlserver_paging_sorting_filtering_and_values_work() {
    let Some(url) = test_url() else { return };
    let pool = DbPool::open_mssql(&url).await.unwrap();
    run_all(
        &pool,
        &[
            "DROP TABLE IF EXISTS dbo.fruits_page",
            "CREATE TABLE dbo.fruits_page (
                id int IDENTITY(1,1) NOT NULL PRIMARY KEY,
                name nvarchar(50) NOT NULL,
                weight float NULL,
                data varbinary(10) NULL
            )",
            "INSERT INTO dbo.fruits_page (name, weight, data) VALUES
                (N'apple', 1.5, 0x0102),
                (N'banana', NULL, NULL),
                (N'a_c', 2.5, NULL),
                (N'avocado', 0.5, NULL)",
        ],
    )
    .await;

    // Sorted paging (ORDER BY … OFFSET/FETCH path).
    let mut request = PageRequest {
        limit: 2,
        sort: Some(("name".into(), SortDir::Asc)),
        ..page_request("fruits_page")
    };
    assert_eq!(pool.count_rows(&request).await.unwrap(), 4);
    let page = pool.fetch_page(&request).await.unwrap();
    assert_eq!(page.rows.len(), 2);
    assert_eq!(page.rows[0][1], Value::Text("a_c".into()));
    assert_eq!(page.rows[1][1], Value::Text("apple".into()));
    request.offset = 2;
    let page = pool.fetch_page(&request).await.unwrap();
    assert_eq!(page.rows[0][1], Value::Text("avocado".into()));
    assert_eq!(page.rows[1][1], Value::Text("banana".into()));

    // Unsorted paging exercises the synthetic ORDER BY (SELECT NULL)
    // OFFSET/FETCH tail T-SQL requires.
    let unsorted = PageRequest {
        limit: 2,
        offset: 1,
        ..page_request("fruits_page")
    };
    assert_eq!(pool.fetch_page(&unsorted).await.unwrap().rows.len(), 2);
    let all = page_request("fruits_page");
    assert_eq!(pool.fetch_page(&all).await.unwrap().rows.len(), 4);

    // Contains filter with an underscore matches literally, not as a wildcard.
    let mut request = page_request("fruits_page");
    request.filter = Some(Filter::contains("name", "a_"));
    let filtered = pool.fetch_page(&request).await.unwrap();
    assert_eq!(filtered.rows.len(), 1);
    assert_eq!(filtered.rows[0][1], Value::Text("a_c".into()));

    // Equals filter on a numeric column via the nvarchar cast; int, nvarchar,
    // float, varbinary, and NULL all decode.
    request.filter = Some(Filter::equals("id", "1"));
    let by_id = pool.fetch_page(&request).await.unwrap();
    assert_eq!(by_id.rows.len(), 1);
    assert_eq!(by_id.rows[0][0], Value::Integer(1));
    assert_eq!(by_id.rows[0][1], Value::Text("apple".into()));
    assert_eq!(by_id.rows[0][2], Value::Real(1.5));
    assert_eq!(by_id.rows[0][3], Value::Blob(vec![1, 2]));
    request.filter = Some(Filter::equals("id", "2"));
    let with_nulls = pool.fetch_page(&request).await.unwrap();
    assert_eq!(with_nulls.rows[0][2], Value::Null);
    assert_eq!(with_nulls.rows[0][3], Value::Null);

    run_all(&pool, &["DROP TABLE dbo.fruits_page"]).await;
    pool.close().await;
}

#[tokio::test]
async fn sqlserver_rich_types_render_correctly() {
    let Some(url) = test_url() else { return };
    let pool = DbPool::open_mssql(&url).await.unwrap();
    run_all(
        &pool,
        &[
            "DROP TABLE IF EXISTS dbo.rich_types_probe",
            "CREATE TABLE dbo.rich_types_probe (
                id int NOT NULL PRIMARY KEY,
                flag bit,
                num decimal(30,10),
                cash money,
                ts2 datetime2,
                ts2_frac datetime2,
                legacy datetime,
                d date,
                t time(3),
                dto datetimeoffset,
                uid uniqueidentifier,
                x xml
            )",
            "INSERT INTO dbo.rich_types_probe VALUES (
                1,
                1,
                123456789012345678.0987654321,
                12.34,
                '2024-03-05 07:08:09',
                '2024-03-05 07:08:09.5000000',
                '2024-03-05 07:08:09.337',
                '2024-03-05',
                '07:08:09.250',
                '2024-03-05 07:08:09 +02:00',
                'A0EEBC99-9C0B-4EF8-BB6D-6BB9BD380A11',
                '<a>1</a>'
            )",
        ],
    )
    .await;

    let result = pool
        .query("SELECT * FROM dbo.rich_types_probe ORDER BY id")
        .await
        .unwrap();
    assert_eq!(result.rows.len(), 1);
    let row = &result.rows[0];
    let col = |name: &str| {
        let idx = result
            .columns
            .iter()
            .position(|c| c.name == name)
            .unwrap_or_else(|| panic!("no column {name}"));
        &row[idx]
    };

    // bit renders as 0/1 — it IS numeric in T-SQL.
    assert_eq!(*col("flag"), Value::Integer(1));
    // decimal with 28 significant digits survives exactly (i128-backed, never
    // through f64); money keeps its full 4-digit scale.
    assert_eq!(
        *col("num"),
        Value::Text("123456789012345678.0987654321".into())
    );
    assert_eq!(*col("cash"), Value::Text("12.3400".into()));
    // Date/time family: fractional seconds only when present, trailing zeros
    // trimmed; legacy datetime rounds to its millisecond display precision.
    assert_eq!(*col("ts2"), Value::Text("2024-03-05 07:08:09".into()));
    assert_eq!(
        *col("ts2_frac"),
        Value::Text("2024-03-05 07:08:09.5".into())
    );
    assert_eq!(
        *col("legacy"),
        Value::Text("2024-03-05 07:08:09.337".into())
    );
    assert_eq!(*col("d"), Value::Text("2024-03-05".into()));
    assert_eq!(*col("t"), Value::Text("07:08:09.25".into()));
    // datetimeoffset keeps its stored offset (it is real data).
    assert_eq!(*col("dto"), Value::Text("2024-03-05 07:08:09+02:00".into()));
    // uniqueidentifier is hyphenated lowercase regardless of input case.
    assert_eq!(
        *col("uid"),
        Value::Text("a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11".into())
    );
    assert_eq!(*col("x"), Value::Text("<a>1</a>".into()));

    run_all(&pool, &["DROP TABLE dbo.rich_types_probe"]).await;
    pool.close().await;
}

#[tokio::test]
async fn sqlserver_staged_edits_round_trip_and_identity_stays_server_assigned() {
    let Some(url) = test_url() else { return };
    let pool = DbPool::open_mssql(&url).await.unwrap();
    run_all(
        &pool,
        &[
            "DROP TABLE IF EXISTS dbo.staged_items",
            "CREATE TABLE dbo.staged_items (
                id int IDENTITY(1,1) NOT NULL PRIMARY KEY,
                title nvarchar(100) NOT NULL,
                qty int NULL
            )",
            "INSERT INTO dbo.staged_items (title, qty) VALUES (N'first', 1), (N'second', 2)",
        ],
    )
    .await;

    let table = introspect_table(&pool, "staged_items").await;
    // The identity column is read-only in the editor…
    let id = table.columns.iter().find(|c| c.name == "id").unwrap();
    assert_eq!(id.generated, Generated::Always);
    let identity = detect_row_identity(&table, Dialect::SqlServer).unwrap();

    // …so the staged insert carries only the writable columns; two edits of
    // one row collapse into a single guarded UPDATE.
    let changes = vec![
        StagedChange::Update {
            locator: RowLocator {
                identity_values: vec![Value::Integer(1)],
            },
            column: "title".into(),
            value: Value::Text("renamed".into()),
        },
        StagedChange::Update {
            locator: RowLocator {
                identity_values: vec![Value::Integer(1)],
            },
            column: "qty".into(),
            value: Value::Integer(7),
        },
        StagedChange::Insert {
            columns: vec!["title".into(), "qty".into()],
            values: vec![Value::Text("third".into()), Value::Integer(3)],
        },
        StagedChange::Delete {
            locator: RowLocator {
                identity_values: vec![Value::Integer(2)],
            },
        },
    ];
    let counts = apply_staged(
        &pool,
        &pool.backend_access(&table),
        &table,
        &identity,
        &changes,
    )
    .await
    .unwrap();
    assert_eq!(counts.updated_rows, 1);
    assert_eq!(counts.inserted_rows, 1);
    assert_eq!(counts.deleted_rows, 1);

    // The server assigned the inserted row's identity value (3 — the next
    // IDENTITY value after the two seed rows).
    let result = pool
        .query("SELECT id, title, qty FROM dbo.staged_items ORDER BY id")
        .await
        .unwrap();
    assert_eq!(
        result.rows,
        vec![
            vec![
                Value::Integer(1),
                Value::Text("renamed".into()),
                Value::Integer(7),
            ],
            vec![
                Value::Integer(3),
                Value::Text("third".into()),
                Value::Integer(3),
            ],
        ]
    );

    run_all(&pool, &["DROP TABLE dbo.staged_items"]).await;
    pool.close().await;
}

#[tokio::test]
async fn sqlserver_staged_row_count_mismatch_rolls_the_batch_back() {
    let Some(url) = test_url() else { return };
    let pool = DbPool::open_mssql(&url).await.unwrap();
    run_all(
        &pool,
        &[
            "DROP TABLE IF EXISTS dbo.staged_guard",
            "CREATE TABLE dbo.staged_guard (
                id int NOT NULL PRIMARY KEY,
                title nvarchar(100) NOT NULL
            )",
            "INSERT INTO dbo.staged_guard VALUES (1, N'first')",
        ],
    )
    .await;

    let table = introspect_table(&pool, "staged_guard").await;
    let identity = detect_row_identity(&table, Dialect::SqlServer).unwrap();

    // A valid edit followed by one addressing a vanished row: the mismatch
    // must roll the WHOLE batch back, valid edit included.
    let changes = vec![
        StagedChange::Update {
            locator: RowLocator {
                identity_values: vec![Value::Integer(1)],
            },
            column: "title".into(),
            value: Value::Text("changed".into()),
        },
        StagedChange::Update {
            locator: RowLocator {
                identity_values: vec![Value::Integer(999)],
            },
            column: "title".into(),
            value: Value::Text("ghost".into()),
        },
    ];
    let err = apply_staged(
        &pool,
        &pool.backend_access(&table),
        &table,
        &identity,
        &changes,
    )
    .await
    .expect_err("a zero-row update must fail the batch");
    assert_eq!(err.change_index, Some(1));
    assert!(
        err.message.contains("expected 1"),
        "unexpected message: {}",
        err.message
    );

    let result = pool
        .query("SELECT title FROM dbo.staged_guard WHERE id = 1")
        .await
        .unwrap();
    assert_eq!(result.rows[0][0], Value::Text("first".into()));

    run_all(&pool, &["DROP TABLE dbo.staged_guard"]).await;
    pool.close().await;
}

#[tokio::test]
async fn sqlserver_script_go_batches_split_and_execute() {
    let Some(url) = test_url() else { return };
    let pool = DbPool::open_mssql(&url).await.unwrap();
    run_all(&pool, &["DROP TABLE IF EXISTS dbo.script_go_probe"]).await;

    // GO is the SSMS/sqlcmd batch separator, not T-SQL — the splitter treats
    // each batch as one statement on the SqlServer dialect.
    let sql = "CREATE TABLE dbo.script_go_probe (id int NOT NULL, note nvarchar(20) NOT NULL)\n\
               GO\n\
               INSERT INTO dbo.script_go_probe VALUES (1, N'one')\n\
               GO\n\
               SELECT id, note FROM dbo.script_go_probe ORDER BY id";
    let statements = split_statements(sql, pool.dialect());
    assert_eq!(statements.len(), 3);

    let mut results = Vec::new();
    run_script(&pool, pool.backend_capabilities(), &statements, |r| {
        results.push(r)
    })
    .await
    .expect("the script should run to completion");
    assert_eq!(results.len(), 3);
    assert!(matches!(results[0].outcome, StatementOutcome::Affected(_)));
    assert_eq!(results[1].outcome, StatementOutcome::Affected(1));
    match &results[2].outcome {
        StatementOutcome::Rows(rows) => {
            assert_eq!(
                rows.rows,
                vec![vec![Value::Integer(1), Value::Text("one".into())]]
            );
        }
        other => panic!("expected rows from the SELECT, got {other:?}"),
    }

    // The multi-statement script ran atomically and committed as a unit.
    let count = pool
        .query("SELECT COUNT(*) FROM dbo.script_go_probe")
        .await
        .unwrap();
    assert_eq!(count.rows[0][0], Value::Integer(1));

    run_all(&pool, &["DROP TABLE dbo.script_go_probe"]).await;
    pool.close().await;
}

#[tokio::test]
async fn sqlserver_script_errors_roll_the_whole_script_back() {
    let Some(url) = test_url() else { return };
    let pool = DbPool::open_mssql(&url).await.unwrap();
    run_all(&pool, &["DROP TABLE IF EXISTS dbo.script_rb_probe"]).await;

    // The third batch violates the PK; the whole script — the CREATE TABLE
    // included — must roll back.
    let sql = "CREATE TABLE dbo.script_rb_probe (id int NOT NULL PRIMARY KEY)\n\
               GO\n\
               INSERT INTO dbo.script_rb_probe VALUES (1)\n\
               GO\n\
               INSERT INTO dbo.script_rb_probe VALUES (1)";
    let statements = split_statements(sql, pool.dialect());
    assert_eq!(statements.len(), 3);

    let mut results = Vec::new();
    let err = run_script(&pool, pool.backend_capabilities(), &statements, |r| {
        results.push(r)
    })
    .await
    .expect_err("the duplicate key must fail the script");
    assert_eq!(err.statement_index, 2);
    assert_eq!(
        err.rollback,
        Rollback::Full,
        "an atomic script failure must roll back, schema changes included"
    );
    assert_eq!(results.len(), 2, "the first two statements had succeeded");

    // Nothing survived — not even the CREATE TABLE (DDL is transactional).
    let gone = pool
        .query("SELECT OBJECT_ID(N'dbo.script_rb_probe')")
        .await
        .unwrap();
    assert_eq!(gone.rows[0][0], Value::Null);

    pool.close().await;
}

#[tokio::test]
async fn sqlserver_query_capped_stops_and_bounds_cells() {
    let Some(url) = test_url() else { return };
    let pool = DbPool::open_mssql(&url).await.unwrap();

    // Row cap: 25 rows exist, ask for 10.
    let (result, truncated) = pool
        .query_capped(
            "SELECT TOP (25) ROW_NUMBER() OVER (ORDER BY object_id) AS g FROM sys.all_objects",
            &[],
            10,
        )
        .await
        .unwrap();
    assert_eq!(result.rows.len(), 10);
    assert!(truncated);

    // Under the cap: no truncation.
    let (result, truncated) = pool
        .query_capped(
            "SELECT TOP (5) ROW_NUMBER() OVER (ORDER BY object_id) AS g FROM sys.all_objects",
            &[],
            100,
        )
        .await
        .unwrap();
    assert_eq!(result.rows.len(), 5);
    assert!(!truncated);

    // Huge cell capped.
    let (result, _t) = pool
        .query_capped(
            "SELECT REPLICATE(CAST(N'Z' AS nvarchar(max)), 200000) AS v",
            &[],
            10,
        )
        .await
        .unwrap();
    if let Value::Text(t) = &result.rows[0][0] {
        assert!(t.len() <= QUERY_CELL_CAP, "cell capped, got {}", t.len());
    } else {
        panic!("expected text");
    }

    pool.close().await;
}

/// A zero-row result still carries its column headers (FRE-138). This backend
/// has always done so — TDS sends result-set metadata even for zero rows — and
/// is the reference the sqlx backends were brought in line with, so pinning it
/// here keeps the contract from drifting back apart.
#[tokio::test]
async fn sqlserver_empty_results_keep_their_column_headers() {
    let Some(url) = test_url() else { return };
    let pool = DbPool::open_mssql(&url).await.unwrap();
    run_all(
        &pool,
        &[
            "DROP TABLE IF EXISTS dbo.empty_headers",
            "CREATE TABLE dbo.empty_headers (id int NOT NULL, name nvarchar(50) NULL)",
        ],
    )
    .await;

    let sql = "SELECT id, name FROM dbo.empty_headers WHERE 1 = 0";
    let empty = pool.query(sql).await.unwrap();
    assert!(empty.rows.is_empty());
    let names: Vec<&str> = empty.columns.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, ["id", "name"]);

    let (empty, truncated) = pool.query_capped(sql, &[], 100).await.unwrap();
    assert!(empty.rows.is_empty());
    assert!(!truncated);
    let names: Vec<&str> = empty.columns.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, ["id", "name"]);

    run_all(&pool, &["DROP TABLE IF EXISTS dbo.empty_headers"]).await;
    pool.close().await;
}

#[tokio::test]
async fn sqlserver_export_streams_csv_and_json() {
    let Some(url) = test_url() else { return };
    let pool = DbPool::open_mssql(&url).await.unwrap();
    run_all(
        &pool,
        &[
            "DROP TABLE IF EXISTS dbo.export_rows",
            "CREATE TABLE dbo.export_rows (
                id int NOT NULL PRIMARY KEY,
                name nvarchar(50) NOT NULL,
                weight float NULL,
                data varbinary(10) NULL
            )",
            "INSERT INTO dbo.export_rows VALUES
                (1, N'plum, \"ripe\"', 1.5, 0x0102),
                (2, N'kiwi', NULL, NULL)",
        ],
    )
    .await;
    let sql = "SELECT id, name, weight, data FROM dbo.export_rows ORDER BY id";

    // CSV: quoted-when-needed fields, \x… blobs, empty NULLs.
    let mut csv = Vec::new();
    let written = pool
        .export(sql, &[], ExportFormat::Csv, &mut csv)
        .await
        .unwrap();
    assert_eq!(written, 2);
    assert_eq!(
        String::from_utf8(csv).unwrap(),
        "id,name,weight,data\n1,\"plum, \"\"ripe\"\"\",1.5,\\x0102\n2,kiwi,,\n"
    );

    // JSON: one object per row, real JSON nulls and numbers.
    let mut json = Vec::new();
    let written = pool
        .export(sql, &[], ExportFormat::Json, &mut json)
        .await
        .unwrap();
    assert_eq!(written, 2);
    assert_eq!(
        String::from_utf8(json).unwrap(),
        "[\n  {\"id\":1,\"name\":\"plum, \\\"ripe\\\"\",\"weight\":1.5,\"data\":\"\\\\x0102\"},\n  \
         {\"id\":2,\"name\":\"kiwi\",\"weight\":null,\"data\":null}\n]\n"
    );

    // Zero rows still yield the CSV header / an empty JSON array (TDS sends
    // result-set metadata even without rows).
    let empty_sql = "SELECT id, name FROM dbo.export_rows WHERE id = 0";
    let mut csv = Vec::new();
    assert_eq!(
        pool.export(empty_sql, &[], ExportFormat::Csv, &mut csv)
            .await
            .unwrap(),
        0
    );
    assert_eq!(String::from_utf8(csv).unwrap(), "id,name\n");
    let mut json = Vec::new();
    pool.export(empty_sql, &[], ExportFormat::Json, &mut json)
        .await
        .unwrap();
    assert_eq!(String::from_utf8(json).unwrap(), "[]\n");

    run_all(&pool, &["DROP TABLE dbo.export_rows"]).await;
    pool.close().await;
}

#[tokio::test]
async fn sqlserver_bounded_page_previews_large_text_and_binary() {
    let Some(url) = test_url() else { return };
    let pool = DbPool::open_mssql(&url).await.unwrap();
    run_all(
        &pool,
        &[
            "DROP TABLE IF EXISTS dbo.bounded_docs",
            "CREATE TABLE dbo.bounded_docs (
                id int NOT NULL PRIMARY KEY,
                small_note nvarchar(50) NULL,
                big_text nvarchar(max) NULL,
                payload varbinary(max) NULL
            )",
            // A 50 000-char text and a 40 000-byte varbinary.
            "INSERT INTO dbo.bounded_docs VALUES
                (1, N'hi', REPLICATE(CAST(N'A' AS nvarchar(max)), 50000),
                 CAST(REPLICATE(CAST('X' AS varchar(max)), 40000) AS varbinary(max))),
                (2, N'yo', N'short enough', 0x0102)",
        ],
    )
    .await;

    let docs = introspect_table(&pool, "bounded_docs").await;
    let request = PageRequest {
        sort: Some(("id".into(), SortDir::Asc)),
        ..page_request("bounded_docs")
    };

    let page = pool
        .fetch_page_bounded(&request, &docs.columns, &["id"])
        .await
        .unwrap();
    assert_eq!(page.result.columns.len(), 4, "length columns stripped");
    // big_text previewed with the real length; varbinary previewed as size.
    let text_preview = page.previews[0][2].expect("big_text truncated");
    assert_eq!(text_preview.full_len, 50_000);
    assert!(!text_preview.binary);
    if let Value::Text(t) = &page.result.rows[0][2] {
        assert!(t.chars().count() <= PREVIEW_BYTES);
    } else {
        panic!("expected text preview");
    }
    let blob_preview = page.previews[0][3].expect("payload truncated");
    assert_eq!(blob_preview.full_len, 40_000);
    assert!(blob_preview.binary);
    // Short values are complete.
    assert!(page.previews[0][1].is_none());
    assert!(page.previews[1][2].is_none());
    assert!(page.previews[1][3].is_none());

    // fetch_cell returns the full text and the full blob.
    let identity = detect_row_identity(&docs, pool.dialect()).unwrap();
    let locator = RowLocator {
        identity_values: vec![Value::Integer(1)],
    };
    let cell = pool
        .fetch_cell(&docs, &identity, &locator, "big_text")
        .await
        .unwrap();
    assert_eq!(cell.full_len, 50_000);
    assert!(!cell.capped);
    if let Value::Text(t) = &cell.value {
        assert_eq!(t.chars().count(), 50_000);
    } else {
        panic!("expected full text");
    }
    let cell = pool
        .fetch_cell(&docs, &identity, &locator, "payload")
        .await
        .unwrap();
    assert_eq!(cell.full_len, 40_000);
    assert!(!cell.capped);
    if let Value::Blob(b) = &cell.value {
        assert_eq!(b.len(), 40_000);
    } else {
        panic!("expected full blob");
    }

    run_all(&pool, &["DROP TABLE dbo.bounded_docs"]).await;
    pool.close().await;
}

#[tokio::test]
async fn sqlserver_trailing_spaces_still_count_toward_the_preview_length() {
    // Regression (FRE-110): the bounded reader's length probe has to count the
    // same units `SUBSTRING` slices by, or a truncated prefix is recorded as a
    // complete value. `LEN()` ignores trailing spaces and `SUBSTRING` does
    // not, so a value that is over the cap only *because* of its padding used
    // to come back as a silent 2048-character prefix carrying no
    // `PreviewInfo` — which the grid then copies to the clipboard, and worse,
    // saves back over the real data on an inline edit.
    let Some(url) = test_url() else { return };
    let pool = DbPool::open_mssql(&url).await.unwrap();
    let body = PREVIEW_BYTES - 48;
    let padding = 1000;
    let total = (body + padding) as u64;
    run_all(
        &pool,
        &[
            "DROP TABLE IF EXISTS dbo.padded_text",
            "CREATE TABLE dbo.padded_text (id int NOT NULL PRIMARY KEY, v nvarchar(max) NULL)",
            &format!(
                "INSERT INTO dbo.padded_text VALUES (1, CONCAT(
                     REPLICATE(CAST(N'x' AS nvarchar(max)), {body}),
                     REPLICATE(CAST(N' ' AS nvarchar(max)), {padding})))"
            ),
        ],
    )
    .await;

    let padded = introspect_table(&pool, "padded_text").await;
    let page = pool
        .fetch_page_bounded(&page_request("padded_text"), &padded.columns, &["id"])
        .await
        .unwrap();
    // The probe must see all the characters, not the `body` count `LEN` reports.
    let preview = page.previews[0][1].expect("a padded over-cap value is a preview");
    assert_eq!(preview.full_len, total);
    let Value::Text(prefix) = &page.result.rows[0][1] else {
        panic!("expected a text preview");
    };
    assert_eq!(prefix.chars().count(), PREVIEW_BYTES);

    // …and the on-demand fetch returns every trailing space.
    let identity = detect_row_identity(&padded, pool.dialect()).unwrap();
    let locator = RowLocator {
        identity_values: vec![Value::Integer(1)],
    };
    let cell = pool
        .fetch_cell(&padded, &identity, &locator, "v")
        .await
        .unwrap();
    assert_eq!(cell.full_len, total);
    assert!(!cell.capped);
    let Value::Text(full) = &cell.value else {
        panic!("expected the full text");
    };
    assert_eq!(full.chars().count(), total as usize);
    assert_eq!(full.len() - full.trim_end().len(), padding);

    run_all(&pool, &["DROP TABLE dbo.padded_text"]).await;
    pool.close().await;
}

#[tokio::test]
async fn sqlserver_sql_variant_cells_browse_and_fetch_safely() {
    let Some(url) = test_url() else { return };
    let pool = DbPool::open_mssql(&url).await.unwrap();
    run_all(
        &pool,
        &[
            "DROP TABLE IF EXISTS dbo.variant_probe",
            "CREATE TABLE dbo.variant_probe (
                id int NOT NULL PRIMARY KEY,
                v sql_variant NULL
            )",
            // One INSERT per row: a multi-row VALUES constructor would unify
            // the variants' underlying types instead of keeping them mixed.
            "INSERT INTO dbo.variant_probe VALUES (1, CAST(42 AS int))",
            "INSERT INTO dbo.variant_probe VALUES (2, CAST(N'txt' AS nvarchar(10)))",
            "INSERT INTO dbo.variant_probe VALUES (3, NULL)",
        ],
    )
    .await;

    let probe = introspect_table(&pool, "variant_probe").await;
    let v = probe.columns.iter().find(|c| c.name == "v").unwrap();
    assert_eq!(v.type_name, "sql_variant");

    // The bounded page path casts sql_variant to nvarchar server-side, so
    // browsing never errors regardless of what the variant holds.
    let request = PageRequest {
        sort: Some(("id".into(), SortDir::Asc)),
        ..page_request("variant_probe")
    };
    let page = pool
        .fetch_page_bounded(&request, &probe.columns, &["id"])
        .await
        .unwrap();
    assert_eq!(page.result.rows[0][1], Value::Text("42".into()));
    assert_eq!(page.result.rows[1][1], Value::Text("txt".into()));
    assert_eq!(page.result.rows[2][1], Value::Null);

    // fetch_cell goes through the same cast and stays safe too.
    let identity = detect_row_identity(&probe, pool.dialect()).unwrap();
    let locator = RowLocator {
        identity_values: vec![Value::Integer(1)],
    };
    let cell = pool
        .fetch_cell(&probe, &identity, &locator, "v")
        .await
        .unwrap();
    assert_eq!(cell.value, Value::Text("42".into()));
    assert_eq!(cell.full_len, 2);
    assert!(!cell.capped);

    run_all(&pool, &["DROP TABLE dbo.variant_probe"]).await;
    pool.close().await;
}

#[tokio::test]
async fn sqlserver_table_stats_come_from_the_partition_counters() {
    let Some(url) = test_url() else { return };
    let pool = DbPool::open_mssql(&url).await.unwrap();
    run_all(
        &pool,
        &[
            "IF OBJECT_ID('dbo.stats_idx_view') IS NOT NULL DROP VIEW dbo.stats_idx_view",
            "IF OBJECT_ID('dbo.stats_view') IS NOT NULL DROP VIEW dbo.stats_view",
            "IF OBJECT_ID('dbo.stats_rows') IS NOT NULL DROP TABLE dbo.stats_rows",
            "CREATE TABLE dbo.stats_rows (id int IDENTITY(1,1) PRIMARY KEY, name nvarchar(50) NOT NULL)",
            "INSERT INTO dbo.stats_rows (name) VALUES ('a'), ('b'), ('c')",
            "CREATE INDEX ix_stats_rows_name ON dbo.stats_rows (name)",
        ],
    )
    .await;

    // sys.dm_db_partition_stats is maintained as rows are written, so unlike
    // Postgres there is nothing to ANALYZE first — but the number is still
    // documented as approximate, so it must still arrive labelled.
    let meta = introspect_table(&pool, "stats_rows").await;
    // Summed over index_id 0/1 only: `ix_stats_rows_name` holds the same three
    // rows at index_id 2, so an unfiltered SUM would report six and this
    // assertion is what pins the filter.
    let stats = pool.fetch_table_stats(&meta).await.unwrap();
    assert_eq!(stats.rows, Some(RowCount::Estimated(3)));
    assert_ne!(
        stats.rows,
        Some(RowCount::Exact(3)),
        "a maintained counter is still not a COUNT(*)"
    );
    assert!(
        stats.bytes.is_some_and(|b| b > 0),
        "reserved pages must be reported: {:?}",
        stats.bytes
    );
    assert_eq!(pool.count_table_rows(&meta).await.unwrap(), 3);

    // A plain view owns no partitions, so both sums are over zero rows and
    // come back absent rather than as zeroes.
    run_all(
        &pool,
        // CREATE VIEW must lead its own batch, which the driver's RPC path
        // cannot provide — EXEC() gives it one.
        &["EXEC('CREATE VIEW dbo.stats_view AS SELECT id, name FROM dbo.stats_rows')"],
    )
    .await;
    let view = pool
        .fetch_table_stats(&introspect_table(&pool, "stats_view").await)
        .await
        .unwrap();
    assert!(
        view.is_empty(),
        "a non-indexed view has no partitions to report: {view:?}"
    );
    assert_eq!(
        pool.count_table_rows(&introspect_table(&pool, "stats_view").await)
            .await
            .unwrap(),
        3
    );

    // An indexed view does own partitions, and nothing here filters views out
    // — the catalog decides, so this one reports real numbers.
    run_all(
        &pool,
        &[
            "EXEC('CREATE VIEW dbo.stats_idx_view WITH SCHEMABINDING AS \
             SELECT id, name FROM dbo.stats_rows')",
            "CREATE UNIQUE CLUSTERED INDEX ix_stats_idx_view ON dbo.stats_idx_view (id)",
        ],
    )
    .await;
    let indexed = pool
        .fetch_table_stats(&introspect_table(&pool, "stats_idx_view").await)
        .await
        .unwrap();
    assert_eq!(
        indexed.rows,
        Some(RowCount::Estimated(3)),
        "an indexed view materializes its rows and counts them"
    );
    assert!(indexed.bytes.is_some_and(|b| b > 0));

    run_all(
        &pool,
        &[
            "DROP VIEW dbo.stats_idx_view",
            "DROP VIEW dbo.stats_view",
            "DROP TABLE dbo.stats_rows",
        ],
    )
    .await;
    pool.close().await;
}

#[tokio::test]
async fn sqlserver_an_empty_table_reports_zero_rows_not_nothing() {
    let Some(url) = test_url() else { return };
    let pool = DbPool::open_mssql(&url).await.unwrap();
    run_all(
        &pool,
        &[
            "IF OBJECT_ID('dbo.stats_empty') IS NOT NULL DROP TABLE dbo.stats_empty",
            "CREATE TABLE dbo.stats_empty (id int NOT NULL PRIMARY KEY, name nvarchar(50))",
        ],
    )
    .await;

    // The counterpart to `an_analyzed_empty_table_reports_zero_rows_rather_
    // than_nothing` in the Postgres suite, and the reason both exist: an empty
    // table must describe itself the same way on both backends. Here there is
    // no ANALYZE to wait for — the partition counter is maintained — so an
    // empty table reads 0 from the moment it is created.
    let meta = introspect_table(&pool, "stats_empty").await;
    let stats = pool.fetch_table_stats(&meta).await.unwrap();
    assert_eq!(
        stats.rows,
        Some(RowCount::Estimated(0)),
        "a maintained counter reading zero is a measurement, not an absence"
    );
    assert!(!stats.is_empty());
    assert_eq!(pool.count_table_rows(&meta).await.unwrap(), 0);

    run_all(&pool, &["DROP TABLE dbo.stats_empty"]).await;
    pool.close().await;
}

#[tokio::test]
async fn sqlserver_offers_no_plan_view_rather_than_a_dangerous_one() {
    let Some(url) = test_url() else { return };
    let pool = DbPool::open_mssql(&url).await.unwrap();

    // T-SQL has no `EXPLAIN` statement (FRE-119). Its estimated plan comes
    // from `SET SHOWPLAN_XML ON`, a session setting that must stand alone in
    // its batch and makes the *next* batch return a plan instead of running
    // it — and hubro's script path hands each statement to the pool
    // separately, so the setting and the statement it is meant to cover can
    // land on different connections. Getting that wrong executes a statement
    // the user asked only to have explained, so the connection declares no
    // plan support at all and the editor disables the action with a reason.
    //
    // Asserted against a live server rather than only against the dialect
    // because this is what the UI reads: a backend that started answering
    // `Some` here would silently start prefixing SQL Server statements with a
    // keyword it does not have.
    assert!(pool.explain_support().is_none());

    pool.close().await;
}
