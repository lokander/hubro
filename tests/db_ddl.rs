//! Integration tests for "Show DDL" (FRE-108).
//!
//! The SQLite half always runs. The Postgres and SQL Server halves need a
//! server (Docker only, per CLAUDE.md) and skip unless `HUBRO_PG_TEST_URL` /
//! `HUBRO_MSSQL_TEST_URL` is set — the `docker run` commands are in the
//! headers of `tests/db_postgres.rs` and `tests/db_sqlserver.rs`.
//!
//! The important assertions are round-trips: create an object, read its DDL,
//! drop it, run the DDL back, and require what comes out to match. A
//! reconstruction that quietly drops a `DEFAULT`, a collation, or an
//! `ON DELETE CASCADE` fails here rather than in someone's migration.
//!
//! Two kinds of round-trip assertion, and the difference matters:
//!
//! - **Re-introspected [`TableMeta`] equality** is the weaker one. `TableMeta`
//!   carries no collation, no check constraints, no referential actions, no
//!   identity seed and no generated-column storage kind, so an equal
//!   `TableMeta` proves considerably less than it reads like it does.
//! - **Regenerated DDL text equality** is the strong one: it compares every
//!   attribute the renderer emits, so anything the reconstruction can express
//!   is covered. Where a table is worth round-tripping at all, it gets this.
//!
//! On top of both, the behavioural checks — a rebuilt `CHECK` must still
//! reject the value it was written to reject — and per-attribute catalog
//! probes, which is how the generated-column storage kind is verified against
//! what the server actually recorded rather than against an assumption.

mod common;

use common::FixtureDb;
use hubro::db::{split_statements, DbPool, DdlObject, DdlSource, TableKind, TableMeta, Value};

fn table<'a>(tables: &'a [TableMeta], schema: Option<&str>, name: &str) -> &'a TableMeta {
    tables
        .iter()
        .find(|t| t.name == name && t.schema.as_deref() == schema)
        .unwrap_or_else(|| panic!("table {name:?} missing from introspection"))
}

/// Runs one statement. Goes through `query` rather than `execute` for the
/// same reason `tests/db_sqlserver.rs` does: tiberius' execute path wraps the
/// batch, and `CREATE VIEW` has to be the first statement of its own batch.
async fn run_one(pool: &DbPool, sql: &str) {
    pool.query(sql)
        .await
        .unwrap_or_else(|e| panic!("running DDL failed: {e}\n---\n{sql}"));
}

/// Runs a DDL text one statement at a time: both sqlx backends use the
/// extended protocol, which rejects multi-statement strings.
async fn run_ddl(pool: &DbPool, sql: &str) {
    for statement in split_statements(sql, pool.dialect()) {
        run_one(pool, &statement).await;
    }
}

/// Every cell of the first result row, for catalog probes.
async fn probe(pool: &DbPool, sql: &str) -> Vec<Vec<Value>> {
    pool.query(sql)
        .await
        .unwrap_or_else(|e| panic!("probe failed: {e}\n---\n{sql}"))
        .rows
}

/// Asserts that `sql` violates a constraint on the rebuilt table. A `CHECK`
/// that survived as *text* but no longer *rejects* anything would pass every
/// metadata comparison in this file.
async fn assert_rejected(pool: &DbPool, sql: &str) {
    let err = pool
        .query(sql)
        .await
        .err()
        .unwrap_or_else(|| panic!("the rebuilt table accepted a row it must reject:\n{sql}"));
    let message = err.to_string().to_lowercase();
    assert!(
        message.contains("constraint") || message.contains("check"),
        "rejected, but not by a constraint: {err}"
    );
}

/// The line of a generated `CREATE TABLE` that defines `column`.
fn column_line<'a>(sql: &'a str, column: &str) -> &'a str {
    let needle = format!("\"{column}\" ");
    sql.lines()
        .map(str::trim)
        .find(|line| line.starts_with(&needle))
        .unwrap_or_else(|| panic!("no line for column {column:?} in:\n{sql}"))
}

// ---------------------------------------------------------------------------
// SQLite — every object has a stored definition, so nothing is reconstructed.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn sqlite_table_ddl_is_the_stored_text_plus_its_indexes() {
    let fixture = FixtureDb::full().await;
    let pool = fixture.open().await;
    let tables = pool.introspect().await.unwrap();

    let ddl = pool
        .fetch_ddl(table(&tables, None, "artists"), &DdlObject::Object)
        .await
        .unwrap();
    assert_eq!(ddl.source, DdlSource::Native);
    // Verbatim, down to the default expression the metadata alone would have
    // had to guess at.
    assert!(ddl.sql.starts_with("CREATE TABLE artists ("));
    assert!(ddl.sql.contains("notes TEXT DEFAULT 'none'"));
    // A table's own indexes travel with it — a CREATE TABLE that silently
    // lost them would be a different table.
    assert!(ddl
        .sql
        .contains("CREATE UNIQUE INDEX idx_artists_name ON artists(name);"));
    // Native output carries no provenance header at all.
    assert_eq!(ddl.text(), ddl.sql);
    assert!(!ddl.text().contains("Reconstructed"));
}

#[tokio::test]
async fn sqlite_view_and_index_ddl_come_back_verbatim() {
    let fixture = FixtureDb::full().await;
    let pool = fixture.open().await;
    let tables = pool.introspect().await.unwrap();

    let view = pool
        .fetch_ddl(table(&tables, None, "artist_overview"), &DdlObject::Object)
        .await
        .unwrap();
    assert_eq!(view.source, DdlSource::Native);
    assert!(view.sql.starts_with("CREATE VIEW artist_overview AS"));
    assert!(view.sql.trim_end().ends_with(';'));

    let index = pool
        .fetch_ddl(
            table(&tables, None, "albums"),
            &DdlObject::Index("idx_albums_title".into()),
        )
        .await
        .unwrap();
    assert_eq!(index.source, DdlSource::Native);
    assert_eq!(index.sql, "CREATE INDEX idx_albums_title ON albums(title);");
}

#[tokio::test]
async fn sqlite_constraint_backed_index_says_where_its_definition_lives() {
    // SQLite creates this index itself for the UNIQUE constraint and stores
    // no CREATE statement for it, so the only honest answer is a labelled
    // rebuild that points at the table definition.
    let fixture = FixtureDb::with_sql("CREATE TABLE t (a TEXT UNIQUE, b INTEGER);").await;
    let pool = fixture.open().await;
    let tables = pool.introspect().await.unwrap();
    let meta = table(&tables, None, "t");
    let name = meta.indexes[0].name.clone();
    assert!(name.starts_with("sqlite_autoindex"));

    let ddl = pool
        .fetch_ddl(meta, &DdlObject::Index(name.clone()))
        .await
        .unwrap();
    assert_eq!(ddl.source, DdlSource::Reconstructed);
    assert_eq!(
        ddl.sql,
        format!("CREATE UNIQUE INDEX \"{name}\" ON \"t\" (\"a\");")
    );
    assert!(ddl.text().contains("UNIQUE / PRIMARY KEY constraint"));
}

#[tokio::test]
async fn sqlite_statements_ending_in_a_line_comment_stay_runnable() {
    // `sqlite_master` stores the statement exactly as written, comment and
    // all. Appending `;` to a trailing `--` line comments the terminator out,
    // and the two indexes then arrive as one unparseable statement.
    let fixture = FixtureDb::with_sql(
        "CREATE TABLE t (a INTEGER, b INTEGER);\n\
         CREATE INDEX ix1 ON t(a) -- first\n;\n\
         CREATE INDEX ix2 ON t(b) -- second\n;",
    )
    .await;
    let pool = fixture.open().await;
    let tables = pool.introspect().await.unwrap();
    let ddl = pool
        .fetch_ddl(table(&tables, None, "t"), &DdlObject::Object)
        .await
        .unwrap();
    assert!(ddl.sql.contains("-- first\n;"), "{}", ddl.sql);

    // The real assertion: the emitted text runs, and rebuilds both indexes.
    let target = FixtureDb::with_sql("").await;
    let target_pool = target.open().await;
    run_ddl(&target_pool, &ddl.sql).await;
    let rebuilt = target_pool.introspect().await.unwrap();
    let mut names: Vec<&str> = table(&rebuilt, None, "t")
        .indexes
        .iter()
        .map(|i| i.name.as_str())
        .collect();
    names.sort_unstable();
    assert_eq!(names, ["ix1", "ix2"]);
}

#[tokio::test]
async fn sqlite_ddl_recreates_the_whole_fixture_schema_identically() {
    let source = FixtureDb::full().await;
    let source_pool = source.open().await;
    let original = source_pool.introspect().await.unwrap();

    // Rebuild the schema in an empty database purely from the DDL, tables
    // first so the view's dependencies exist.
    let target = FixtureDb::with_sql("").await;
    let target_pool = target.open().await;
    let mut ordered: Vec<&TableMeta> = original.iter().collect();
    ordered.sort_by_key(|t| t.kind != TableKind::Table);
    for meta in ordered {
        let ddl = source_pool
            .fetch_ddl(meta, &DdlObject::Object)
            .await
            .unwrap();
        run_ddl(&target_pool, &ddl.sql).await;
    }

    // Same metadata, object for object: columns, keys, indexes, and foreign
    // keys all survived the round trip.
    let rebuilt = target_pool.introspect().await.unwrap();
    assert_eq!(rebuilt, original);
}

// ---------------------------------------------------------------------------
// Postgres — native view/index definitions, reconstructed tables.
// ---------------------------------------------------------------------------

async fn pg_url() -> Option<String> {
    common::pg_test_url().await
}

/// A table with one of everything the reconstruction has to carry: an
/// identity key, a length-carrying type, a non-default collation, a default,
/// a stored generated column, a check, a unique constraint, a cascading
/// foreign key, and a partial index. Deliberately no `serial`: its default
/// references a sequence the rebuild does not create (a declared caveat).
///
/// Every test gets its own `tag` so the suite stays re-runnable and the tests
/// stay independent under cargo's parallel execution.
async fn pg_fixture(pool: &DbPool, tag: &str) {
    for statement in [
        format!("DROP VIEW IF EXISTS ddl_view_{tag}"),
        format!("DROP TABLE IF EXISTS ddl_child_{tag}"),
        format!("DROP TABLE IF EXISTS ddl_parent_{tag}"),
    ] {
        run_one(pool, &statement).await;
    }
    run_ddl(
        pool,
        &format!(
            r#"
CREATE TABLE ddl_parent_{tag} (
    id integer GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    label varchar(40) NOT NULL
);
CREATE TABLE ddl_child_{tag} (
    id integer GENERATED BY DEFAULT AS IDENTITY PRIMARY KEY,
    parent_id integer NOT NULL
        REFERENCES ddl_parent_{tag} (id) ON DELETE CASCADE ON UPDATE CASCADE,
    code text COLLATE "C" NOT NULL,
    amount numeric(10,2) DEFAULT 0.00,
    doubled numeric GENERATED ALWAYS AS (amount * 2) STORED,
    CONSTRAINT ddl_child_amount_positive_{tag} CHECK (amount >= 0),
    CONSTRAINT ddl_child_code_key_{tag} UNIQUE (code)
);
CREATE INDEX ddl_child_big_{tag} ON ddl_child_{tag} (parent_id) WHERE amount > 10;
CREATE VIEW ddl_view_{tag} AS SELECT id, code FROM ddl_child_{tag};
"#
        ),
    )
    .await;
}

#[tokio::test]
async fn postgres_table_ddl_carries_every_attribute_the_catalog_knows() {
    let Some(url) = pg_url().await else { return };
    let pool = DbPool::open_postgres(&url).await.unwrap();
    pg_fixture(&pool, "attrs").await;
    let tables = pool.introspect().await.unwrap();

    let ddl = pool
        .fetch_ddl(
            table(&tables, Some("public"), "ddl_child_attrs"),
            &DdlObject::Object,
        )
        .await
        .unwrap();
    assert_eq!(ddl.source, DdlSource::Reconstructed);
    let sql = &ddl.sql;
    // Exact types — information_schema would have said bare `numeric` and
    // `character varying` here.
    assert!(
        sql.contains("\"amount\" numeric(10,2) DEFAULT 0.00"),
        "{sql}"
    );
    assert!(
        sql.contains("\"code\" text COLLATE \"C\" NOT NULL"),
        "{sql}"
    );
    assert!(
        sql.contains("\"id\" integer GENERATED BY DEFAULT AS IDENTITY NOT NULL"),
        "{sql}"
    );
    assert!(sql.contains("GENERATED ALWAYS AS ((amount * "), "{sql}");
    // The three things TableMeta alone could never have supplied.
    assert!(sql.contains("CHECK ((amount >= "), "{sql}");
    assert!(sql.contains("UNIQUE (code)"), "{sql}");
    assert!(
        sql.contains("ON UPDATE CASCADE ON DELETE CASCADE")
            || sql.contains("ON DELETE CASCADE ON UPDATE CASCADE"),
        "{sql}"
    );
    // The partial index rides along with the table.
    assert!(sql.contains("CREATE INDEX ddl_child_big_attrs"), "{sql}");
    assert!(sql.contains("WHERE (amount > "), "{sql}");
    // And the whole thing is labelled as a rebuild.
    assert!(ddl.text().starts_with("-- Reconstructed by hubro"));
    assert!(ddl.text().contains("sequences behind nextval() defaults"));

    pool.close().await;
}

#[tokio::test]
async fn postgres_table_ddl_round_trips_through_the_server() {
    let Some(url) = pg_url().await else { return };
    let pool = DbPool::open_postgres(&url).await.unwrap();
    pg_fixture(&pool, "trip").await;
    let original = pool.introspect().await.unwrap();
    let meta = table(&original, Some("public"), "ddl_child_trip").clone();

    let ddl = pool.fetch_ddl(&meta, &DdlObject::Object).await.unwrap();
    run_one(&pool, "DROP VIEW ddl_view_trip").await;
    run_one(&pool, "DROP TABLE ddl_child_trip").await;
    // The header is comments, so the copied text runs as-is.
    run_ddl(&pool, &ddl.text()).await;

    // The weak assertion (see the module docs): TableMeta carries no
    // collation, checks, referential actions or generated storage kind.
    let rebuilt = pool.introspect().await.unwrap();
    assert_eq!(table(&rebuilt, Some("public"), "ddl_child_trip"), &meta);

    // The strong one: regenerating the DDL from the rebuilt table must give
    // back the same text, which compares every attribute the renderer emits.
    let regenerated = pool
        .fetch_ddl(
            table(&rebuilt, Some("public"), "ddl_child_trip"),
            &DdlObject::Object,
        )
        .await
        .unwrap();
    assert_eq!(regenerated.text(), ddl.text());

    // And the rebuilt CHECK still rejects what it was written to reject —
    // no metadata comparison in this file can see that.
    run_one(&pool, "INSERT INTO ddl_parent_trip (label) VALUES ('p')").await;
    assert_rejected(
        &pool,
        "INSERT INTO ddl_child_trip (parent_id, code, amount) \
         SELECT id, 'x', -1 FROM ddl_parent_trip LIMIT 1",
    )
    .await;

    pool.close().await;
}

#[tokio::test]
async fn postgres_generated_column_storage_matches_the_catalog() {
    let Some(url) = pg_url().await else { return };
    let pool = DbPool::open_postgres(&url).await.unwrap();
    run_one(&pool, "DROP TABLE IF EXISTS ddl_gen").await;
    run_one(
        &pool,
        "CREATE TABLE ddl_gen (n integer, s integer GENERATED ALWAYS AS (n * 2) STORED)",
    )
    .await;
    // PG 18 made a bare GENERATED ALWAYS AS (…) *virtual* and only accepts
    // this there; on 17 and earlier it is a syntax error, so the failure is
    // ignored and the test still covers the STORED half.
    let _ = pool
        .query("ALTER TABLE ddl_gen ADD COLUMN v integer GENERATED ALWAYS AS (n * 3)")
        .await;

    let tables = pool.introspect().await.unwrap();
    let ddl = pool
        .fetch_ddl(
            table(&tables, Some("public"), "ddl_gen"),
            &DdlObject::Object,
        )
        .await
        .unwrap();

    // Ask the catalog what storage each generated column actually has and
    // require the emitted keyword to match, rather than assuming STORED:
    // rendering a virtual column as STORED silently materializes it.
    let rows = probe(
        &pool,
        "SELECT a.attname::text, a.attgenerated::text \
         FROM pg_attribute a \
         JOIN pg_class c ON c.oid = a.attrelid \
         JOIN pg_namespace n ON n.oid = c.relnamespace \
         WHERE n.nspname = 'public' AND c.relname = 'ddl_gen' AND a.attgenerated <> ''",
    )
    .await;
    assert!(!rows.is_empty(), "no generated columns to check");
    for row in &rows {
        let name = row[0].display();
        let expected = match row[1].display().as_str() {
            "s" => "STORED",
            "v" => "VIRTUAL",
            other => panic!("unhandled pg_attribute.attgenerated {other:?}"),
        };
        let line = column_line(&ddl.sql, &name);
        assert!(line.contains("GENERATED ALWAYS AS ("), "{line}");
        assert!(
            line.trim_end_matches(',').ends_with(expected),
            "column {name} is {expected} in the catalog but hubro emitted: {line}"
        );
    }
    // Whatever the server said, the statement must run back.
    run_one(&pool, "DROP TABLE ddl_gen").await;
    run_ddl(&pool, &ddl.text()).await;
    run_one(&pool, "DROP TABLE ddl_gen").await;

    pool.close().await;
}

#[tokio::test]
async fn postgres_plain_table_claims_no_missing_constraints() {
    let Some(url) = pg_url().await else { return };
    let pool = DbPool::open_postgres(&url).await.unwrap();
    run_one(&pool, "DROP TABLE IF EXISTS ddl_plain").await;
    run_one(&pool, "CREATE TABLE ddl_plain (a integer, b text)").await;
    let tables = pool.introspect().await.unwrap();
    let ddl = pool
        .fetch_ddl(
            table(&tables, Some("public"), "ddl_plain"),
            &DdlObject::Object,
        )
        .await
        .unwrap();

    // The catalog read succeeded and found no constraints, which is not the
    // same as failing to read them. Claiming otherwise on the commonest table
    // shape in a database teaches people to skim past the header.
    let text = ddl.text();
    for lie in [
        "check constraints",
        "unique constraints",
        "foreign-key ON DELETE",
        "constraint names",
    ] {
        assert!(!text.contains(lie), "false caveat {lie:?} in:\n{text}");
    }
    // The standing, genuinely-true caveats are still there.
    assert!(
        text.contains("triggers and row-level security policies"),
        "{text}"
    );
    run_one(&pool, "DROP TABLE ddl_plain").await;

    pool.close().await;
}

#[tokio::test]
async fn postgres_view_and_index_ddl_are_the_servers_own() {
    let Some(url) = pg_url().await else { return };
    let pool = DbPool::open_postgres(&url).await.unwrap();
    pg_fixture(&pool, "native").await;
    let tables = pool.introspect().await.unwrap();

    let view = pool
        .fetch_ddl(
            table(&tables, Some("public"), "ddl_view_native"),
            &DdlObject::Object,
        )
        .await
        .unwrap();
    assert_eq!(view.source, DdlSource::Native);
    assert!(view
        .sql
        .starts_with("CREATE VIEW \"public\".\"ddl_view_native\" AS"));
    // No provenance header on native output.
    assert_eq!(view.text(), view.sql);

    let index = pool
        .fetch_ddl(
            table(&tables, Some("public"), "ddl_child_native"),
            &DdlObject::Index("ddl_child_big_native".into()),
        )
        .await
        .unwrap();
    assert_eq!(index.source, DdlSource::Native);
    assert!(index
        .sql
        .starts_with("CREATE INDEX ddl_child_big_native ON public.ddl_child_native"));
    assert!(index.sql.contains("WHERE (amount > "));

    // Both are runnable exactly as returned.
    run_one(&pool, "DROP VIEW ddl_view_native").await;
    run_one(&pool, "DROP INDEX ddl_child_big_native").await;
    run_ddl(&pool, &view.sql).await;
    run_ddl(&pool, &index.sql).await;

    pool.close().await;
}

// ---------------------------------------------------------------------------
// SQL Server — native view text, reconstructed tables and indexes.
// ---------------------------------------------------------------------------

async fn mssql_url() -> Option<String> {
    common::mssql_test_url().await
}

/// The T-SQL counterpart of [`pg_fixture`], plus the three things only SQL
/// Server has: a non-unit identity seed/increment, a persisted computed
/// column, and a persisted computed column that is `NOT NULL` (T-SQL only
/// accepts the nullability on a persisted one, and dropping it turns the
/// column nullable). Per-test `tag` for the same reason.
async fn mssql_fixture(pool: &DbPool, tag: &str) {
    for statement in [
        format!("DROP VIEW IF EXISTS dbo.ddl_view_{tag}"),
        format!("DROP TABLE IF EXISTS dbo.ddl_child_{tag}"),
        format!("DROP TABLE IF EXISTS dbo.ddl_parent_{tag}"),
    ] {
        run_one(pool, &statement).await;
    }
    run_ddl(
        pool,
        &format!(
            r#"
CREATE TABLE dbo.ddl_parent_{tag} (
    id int IDENTITY(1,1) CONSTRAINT PK_ddl_parent_{tag} PRIMARY KEY,
    label nvarchar(40) NOT NULL
);
CREATE TABLE dbo.ddl_child_{tag} (
    id int IDENTITY(10,5) CONSTRAINT PK_ddl_child_{tag} PRIMARY KEY,
    parent_id int NOT NULL
        CONSTRAINT FK_ddl_child_parent_{tag}
        REFERENCES dbo.ddl_parent_{tag} (id) ON DELETE CASCADE,
    code nvarchar(20) COLLATE Latin1_General_BIN NOT NULL
        CONSTRAINT UQ_ddl_child_code_{tag} UNIQUE,
    amount decimal(10,2) CONSTRAINT DF_ddl_child_amount_{tag} DEFAULT (0),
    qty int NOT NULL CONSTRAINT DF_ddl_child_qty_{tag} DEFAULT (1),
    doubled AS (amount * 2) PERSISTED,
    qty2 AS (qty * 2) PERSISTED NOT NULL,
    CONSTRAINT CK_ddl_child_amount_{tag} CHECK (amount >= 0)
);
CREATE INDEX IX_ddl_child_big_{tag} ON dbo.ddl_child_{tag} (parent_id DESC)
    INCLUDE (code) WHERE amount > 10;
"#
        ),
    )
    .await;
    // CREATE VIEW must lead its own batch, which the driver's RPC path can't
    // provide — EXEC() gives it one (same trick as tests/db_sqlserver.rs).
    run_one(
        pool,
        &format!(
            "EXEC('CREATE VIEW dbo.ddl_view_{tag} AS SELECT id, code FROM dbo.ddl_child_{tag}')"
        ),
    )
    .await;
}

#[tokio::test]
async fn sqlserver_table_ddl_carries_every_attribute_the_catalog_knows() {
    let Some(url) = mssql_url().await else { return };
    let pool = DbPool::open_mssql(&url).await.unwrap();
    mssql_fixture(&pool, "attrs").await;
    let tables = pool.introspect().await.unwrap();

    let ddl = pool
        .fetch_ddl(
            table(&tables, Some("dbo"), "ddl_child_attrs"),
            &DdlObject::Object,
        )
        .await
        .unwrap();
    assert_eq!(ddl.source, DdlSource::Reconstructed);
    let sql = &ddl.sql;
    assert!(sql.contains("\"id\" int IDENTITY(10,5) NOT NULL"), "{sql}");
    // T-SQL collation names are keywords, so they must be unquoted.
    assert!(
        sql.contains("\"code\" nvarchar(20) COLLATE Latin1_General_BIN NOT NULL"),
        "{sql}"
    );
    // The default keeps the name you would need in order to drop it.
    assert!(
        sql.contains("\"amount\" decimal(10,2) CONSTRAINT \"DF_ddl_child_amount_attrs\" DEFAULT 0"),
        "{sql}"
    );
    assert_eq!(
        column_line(sql, "doubled"),
        "\"doubled\" AS ([amount]*(2)) PERSISTED,"
    );
    // A persisted computed column carries its own nullability.
    assert_eq!(
        column_line(sql, "qty2"),
        "\"qty2\" AS ([qty]*(2)) PERSISTED NOT NULL,"
    );
    assert!(
        sql.contains("CONSTRAINT \"PK_ddl_child_attrs\" PRIMARY KEY CLUSTERED (\"id\" ASC)"),
        "{sql}"
    );
    assert!(
        sql.contains("CONSTRAINT \"UQ_ddl_child_code_attrs\" UNIQUE"),
        "{sql}"
    );
    assert!(
        sql.contains("CONSTRAINT \"CK_ddl_child_amount_attrs\" CHECK"),
        "{sql}"
    );
    assert!(sql.contains("ON DELETE CASCADE"), "{sql}");
    // The filtered index with its direction and INCLUDE list.
    assert!(
        sql.contains("CREATE NONCLUSTERED INDEX \"IX_ddl_child_big_attrs\""),
        "{sql}"
    );
    assert!(
        sql.contains("(\"parent_id\" DESC) INCLUDE (\"code\") WHERE"),
        "{sql}"
    );
    assert!(ddl.text().starts_with("-- Reconstructed by hubro"));

    pool.close().await;
}

#[tokio::test]
async fn sqlserver_table_ddl_round_trips_through_the_server() {
    let Some(url) = mssql_url().await else { return };
    let pool = DbPool::open_mssql(&url).await.unwrap();
    mssql_fixture(&pool, "trip").await;
    let original = pool.introspect().await.unwrap();
    let meta = table(&original, Some("dbo"), "ddl_child_trip").clone();

    let ddl = pool.fetch_ddl(&meta, &DdlObject::Object).await.unwrap();
    run_one(&pool, "DROP VIEW dbo.ddl_view_trip").await;
    run_one(&pool, "DROP TABLE dbo.ddl_child_trip").await;
    run_ddl(&pool, &ddl.text()).await;

    // Weak assertion first (see the module docs), then the strong one:
    // regenerating the DDL compares every attribute the renderer emits,
    // including collation, checks, referential actions and identity seed.
    let rebuilt = pool.introspect().await.unwrap();
    assert_eq!(table(&rebuilt, Some("dbo"), "ddl_child_trip"), &meta);
    let regenerated = pool
        .fetch_ddl(
            table(&rebuilt, Some("dbo"), "ddl_child_trip"),
            &DdlObject::Object,
        )
        .await
        .unwrap();
    assert_eq!(regenerated.text(), ddl.text());

    // The identity seed survived as behaviour, not just as text.
    run_one(
        &pool,
        "INSERT INTO dbo.ddl_parent_trip (label) VALUES (N'p')",
    )
    .await;
    run_one(
        &pool,
        "INSERT INTO dbo.ddl_child_trip (parent_id, code, amount) \
         SELECT TOP 1 id, N'x', 1 FROM dbo.ddl_parent_trip",
    )
    .await;
    let ids = probe(&pool, "SELECT id FROM dbo.ddl_child_trip").await;
    assert_eq!(ids[0][0], Value::Integer(10));
    // And the rebuilt CHECK still rejects what it was written to reject.
    assert_rejected(
        &pool,
        "INSERT INTO dbo.ddl_child_trip (parent_id, code, amount) \
         SELECT TOP 1 id, N'y', -1 FROM dbo.ddl_parent_trip",
    )
    .await;

    pool.close().await;
}

#[tokio::test]
async fn sqlserver_plain_table_claims_no_missing_constraints() {
    let Some(url) = mssql_url().await else { return };
    let pool = DbPool::open_mssql(&url).await.unwrap();
    run_one(&pool, "DROP TABLE IF EXISTS dbo.ddl_plain").await;
    run_one(&pool, "CREATE TABLE dbo.ddl_plain (a int, b nvarchar(10))").await;
    let tables = pool.introspect().await.unwrap();
    let ddl = pool
        .fetch_ddl(table(&tables, Some("dbo"), "ddl_plain"), &DdlObject::Object)
        .await
        .unwrap();

    let text = ddl.text();
    for lie in [
        "check constraints",
        "unique constraints",
        "foreign-key ON DELETE",
        "constraint names",
    ] {
        assert!(!text.contains(lie), "false caveat {lie:?} in:\n{text}");
    }
    assert!(text.contains("triggers and system-versioning"), "{text}");
    run_one(&pool, "DROP TABLE dbo.ddl_plain").await;

    pool.close().await;
}

#[tokio::test]
async fn sqlserver_declares_what_it_cannot_express_instead_of_faking_it() {
    let Some(url) = mssql_url().await else { return };
    let pool = DbPool::open_mssql(&url).await.unwrap();
    run_one(&pool, "DROP TABLE IF EXISTS dbo.ddl_exotic").await;
    // An unnamed default (SQL Server invents `DF__ddl_exot__n__…`), a check
    // that is deliberately disabled, and a columnstore index.
    run_ddl(
        &pool,
        "CREATE TABLE dbo.ddl_exotic (n int DEFAULT (7), m int);\n\
         ALTER TABLE dbo.ddl_exotic WITH NOCHECK ADD CONSTRAINT CK_ddl_exotic CHECK (m > 0);\n\
         ALTER TABLE dbo.ddl_exotic NOCHECK CONSTRAINT CK_ddl_exotic;\n\
         CREATE NONCLUSTERED COLUMNSTORE INDEX CSI_ddl_exotic ON dbo.ddl_exotic (n, m);",
    )
    .await;
    let tables = pool.introspect().await.unwrap();
    let meta = table(&tables, Some("dbo"), "ddl_exotic");
    let ddl = pool.fetch_ddl(meta, &DdlObject::Object).await.unwrap();
    let text = ddl.text();

    // The auto-generated default-constraint name is kept, not silently
    // replaced by a fresh random one on the next run.
    let auto_name = probe(
        &pool,
        "SELECT d.name FROM sys.default_constraints d \
         JOIN sys.objects o ON o.object_id = d.parent_object_id \
         WHERE o.name = 'ddl_exotic'",
    )
    .await[0][0]
        .display();
    assert!(auto_name.starts_with("DF__"), "{auto_name}");
    assert!(
        text.contains(&format!("CONSTRAINT \"{auto_name}\" DEFAULT 7")),
        "{text}"
    );

    // A disabled constraint cannot be expressed in CREATE TABLE, so the
    // behaviour change is named rather than performed silently.
    assert!(text.contains("CK_ddl_exotic"), "{text}");
    assert!(
        text.contains("disabled / untrusted state of CK_ddl_exotic"),
        "{text}"
    );

    // A columnstore index has no key list this renderer can produce, so it is
    // declared missing rather than emitted as `CREATE INDEX … ()`.
    assert!(!text.contains("CSI_ddl_exotic\" ON"), "{text}");
    assert!(text.contains("not a rowstore index"), "{text}");
    let refused = pool
        .fetch_ddl(meta, &DdlObject::Index("CSI_ddl_exotic".into()))
        .await
        .expect_err("a columnstore index must be refused, not faked");
    assert!(refused.to_string().contains("rowstore"), "{refused}");

    // Whatever it did emit still runs.
    run_one(&pool, "DROP TABLE dbo.ddl_exotic").await;
    run_ddl(&pool, &text).await;
    run_one(&pool, "DROP TABLE dbo.ddl_exotic").await;

    pool.close().await;
}

#[tokio::test]
async fn sqlserver_view_ddl_is_the_stored_module_text() {
    let Some(url) = mssql_url().await else { return };
    let pool = DbPool::open_mssql(&url).await.unwrap();
    mssql_fixture(&pool, "module").await;
    let tables = pool.introspect().await.unwrap();

    let view = pool
        .fetch_ddl(
            table(&tables, Some("dbo"), "ddl_view_module"),
            &DdlObject::Object,
        )
        .await
        .unwrap();
    assert_eq!(view.source, DdlSource::Native);
    // Byte-for-byte what was submitted, not a normalized rewrite.
    assert_eq!(
        view.sql,
        "CREATE VIEW dbo.ddl_view_module AS SELECT id, code FROM dbo.ddl_child_module"
    );
    assert_eq!(view.text(), view.sql);

    pool.close().await;
}

#[tokio::test]
async fn sqlserver_index_ddl_is_reconstructed_and_runnable() {
    let Some(url) = mssql_url().await else { return };
    let pool = DbPool::open_mssql(&url).await.unwrap();
    mssql_fixture(&pool, "index").await;
    let tables = pool.introspect().await.unwrap();
    let meta = table(&tables, Some("dbo"), "ddl_child_index");

    let ddl = pool
        .fetch_ddl(meta, &DdlObject::Index("IX_ddl_child_big_index".into()))
        .await
        .unwrap();
    assert_eq!(ddl.source, DdlSource::Reconstructed);
    run_one(
        &pool,
        "DROP INDEX IX_ddl_child_big_index ON dbo.ddl_child_index",
    )
    .await;
    run_ddl(&pool, &ddl.text()).await;
    let rebuilt = pool.introspect().await.unwrap();
    assert_eq!(
        table(&rebuilt, Some("dbo"), "ddl_child_index").indexes,
        meta.indexes
    );

    // A constraint's index is reachable from the same list, and says so.
    let pk = pool
        .fetch_ddl(meta, &DdlObject::Index("PK_ddl_child_index".into()))
        .await
        .unwrap();
    assert!(pk
        .text()
        .contains("backs a PRIMARY KEY / UNIQUE constraint"));

    pool.close().await;
}
