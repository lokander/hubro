//! What the Postgres-family engine suites share: lookups, the sensors and
//! readings fixture, and the assertions several engines make the same way.
//!
//! Each engine suite (`db_timescale`, `db_citus`, `db_cockroach`,
//! `db_yugabyte`, `db_materialize`, `db_risingwave`) used to carry its own copy
//! of these. Measured with `diff -w`, the CockroachDB and YugabyteDB paging
//! tests differed by one comment line, so a fix to one copy left the others
//! behind. An assertion here states the claim once; each suite says what
//! differs on its engine (the table, the key, the expected count) and adds the
//! checks that only it makes. `tests/db_fk.rs`'s `assert_fk_navigation` is the
//! pattern.
//!
//! Nothing here introspects (`tests/engine_urls.rs` pins that for all of
//! `tests/common`). An assertion that needs a table's metadata is handed it,
//! so the suite decides how to introspect.

use hubro::db::{
    apply_staged, detect_row_identity, run_script, split_statements, DbPool, Internal, PageRequest,
    QueryResult, Rollback, RowIdentity, RowLocator, SortDir, StagedChange, TableMeta, Value,
};
use hubro::db::{Capabilities, Reading};

/// The `public` table called `name`.
pub fn find<'a>(tables: &'a [TableMeta], name: &str) -> &'a TableMeta {
    tables
        .iter()
        .find(|t| t.name == name && t.schema.as_deref() == Some("public"))
        .unwrap_or_else(|| panic!("{name} missing from introspection"))
}

/// An integer cell's value.
pub fn integer(value: &Value) -> i64 {
    match value {
        Value::Integer(n) => *n,
        other => panic!("expected an integer cell, got {other:?}"),
    }
}

/// A text cell's value.
pub fn text(value: &Value) -> &str {
    match value {
        Value::Text(text) => text,
        other => panic!("expected a text cell, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// The sensors and readings fixture
// ---------------------------------------------------------------------------

/// How the readings table is keyed.
#[derive(Clone, Copy)]
pub enum ReadingsKey {
    /// `PRIMARY KEY (id, sensor_id)`, ids 1 to 8 for each of the two sensors,
    /// temperature `id * 1.5`, and an index on temperature.
    IdAndSensor,
    /// TimescaleDB: `PRIMARY KEY (time, sensor_id)`, made a hypertable with
    /// one-day chunks and given a reading every six hours from 2026-01-01 to
    /// 2026-01-05, so the 17 timestamps per sensor span several chunks.
    TimeHypertable,
}

/// A `sensors` table (ids 1 `alpha` and 2 `beta`) and a `readings` table
/// with a foreign key to it and a nullable `note`, named
/// `{prefix}readings_{suffix}` and `{prefix}sensors_{suffix}`. Suffixed per
/// test so the tests in one binary can run concurrently without fighting over
/// one fixture.
pub struct SensorFixture {
    /// Prepended to both table names.
    pub prefix: &'static str,
    pub key: ReadingsKey,
    /// Held while the schema is (re)built, for an engine that refuses
    /// concurrent DDL. The rows load after it is released: they are ordinary
    /// writes.
    pub ddl_lock: Option<&'static tokio::sync::Mutex<()>>,
}

impl SensorFixture {
    /// Drops and recreates the two tables. Returns `(readings, sensors)`.
    pub async fn create(&self, pool: &DbPool, suffix: &str) -> (String, String) {
        let prefix = self.prefix;
        let readings = format!("{prefix}readings_{suffix}");
        let sensors = format!("{prefix}sensors_{suffix}");
        let mut schema = vec![
            format!("DROP TABLE IF EXISTS {readings} CASCADE"),
            format!("DROP TABLE IF EXISTS {sensors} CASCADE"),
            format!("CREATE TABLE {sensors} (id int PRIMARY KEY, name text NOT NULL)"),
        ];
        let mut rows = vec![format!(
            "INSERT INTO {sensors} (id, name) VALUES (1, 'alpha'), (2, 'beta')"
        )];
        match self.key {
            ReadingsKey::IdAndSensor => {
                schema.push(format!(
                    "CREATE TABLE {readings} (
                        id          int NOT NULL,
                        sensor_id   int NOT NULL REFERENCES {sensors}(id),
                        temperature double precision,
                        note        text,
                        PRIMARY KEY (id, sensor_id)
                    )"
                ));
                schema.push(format!(
                    "CREATE INDEX {readings}_by_temp ON {readings} (temperature)"
                ));
                rows.push(format!(
                    "INSERT INTO {readings} (id, sensor_id, temperature)
                     SELECT g, s.id, g * 1.5 FROM generate_series(1, 8) g CROSS JOIN {sensors} s"
                ));
            }
            ReadingsKey::TimeHypertable => {
                schema.push(format!(
                    "CREATE TABLE {readings} (
                        time        timestamptz NOT NULL,
                        sensor_id   int NOT NULL REFERENCES {sensors}(id),
                        temperature double precision,
                        note        text,
                        PRIMARY KEY (time, sensor_id)
                    )"
                ));
                schema.push(format!(
                    "SELECT create_hypertable('{readings}', by_range('time', INTERVAL '1 day'))"
                ));
                rows.push(format!(
                    "INSERT INTO {readings} (time, sensor_id, temperature)
                     SELECT ts, s.id, extract(epoch from ts) / 1e9
                     FROM generate_series(
                         '2026-01-01'::timestamptz, '2026-01-05'::timestamptz, INTERVAL '6 hours'
                     ) ts
                     CROSS JOIN {sensors} s"
                ));
            }
        }
        {
            let _guard = match self.ddl_lock {
                Some(lock) => Some(lock.lock().await),
                None => None,
            };
            for sql in &schema {
                pool.query_unbounded(sql)
                    .await
                    .unwrap_or_else(|e| panic!("{sql}: {e}"));
            }
        }
        for sql in &rows {
            pool.query_unbounded(sql)
                .await
                .unwrap_or_else(|e| panic!("{sql}: {e}"));
        }
        (readings, sensors)
    }
}

// ---------------------------------------------------------------------------
// Shared assertions
// ---------------------------------------------------------------------------

/// What a filtered count must come to.
pub enum Expected {
    Exactly(u64),
    /// For a fixture whose exact spread is not the point, only that it is
    /// spread (Citus's rows across its shards).
    MoreThan(u64),
}

/// `request` (sorted, filtered, `limit` rows from offset 0) counts as
/// `expected`, returns a full first page in the requested order, and its next
/// page carries on that order rather than restarting it, which is what an
/// engine that fans a query out over chunks or shards and merges the results
/// could get wrong.
///
/// Returns the two pages, for the checks only one engine makes.
pub async fn assert_pages_sorts_and_filters(
    pool: &DbPool,
    request: &PageRequest,
    expected: Expected,
) -> (QueryResult, QueryResult) {
    assert_eq!(request.offset, 0, "the first page starts at offset 0");
    // Two rows at least, or the in-page order check has no pair to look at.
    assert!(request.limit >= 2, "a page of at least two rows");
    let (column, dir) = request.sort.clone().expect("a sorted request");
    let total = pool.count_rows(Reading::UNMARKED, request).await.unwrap();
    match expected {
        Expected::Exactly(n) => assert_eq!(total, n, "filtered count"),
        Expected::MoreThan(n) => assert!(total > n, "expected a spread of rows, got {total}"),
    }

    let page = pool.fetch_page(Reading::UNMARKED, request).await.unwrap();
    assert_eq!(page.rows.len(), request.limit as usize, "a full first page");
    let at = page
        .columns
        .iter()
        .position(|c| c.name == column)
        .unwrap_or_else(|| panic!("the page has no {column} column"));
    let in_order = |a: &Value, b: &Value| {
        let ordering = compare(a, b);
        match dir {
            SortDir::Asc => ordering.is_lt(),
            SortDir::Desc => ordering.is_gt(),
        }
    };
    for pair in page.rows.windows(2) {
        assert!(
            in_order(&pair[0][at], &pair[1][at]),
            "rows should come back by {column} {dir:?}: {:?} then {:?}",
            pair[0][at],
            pair[1][at]
        );
    }

    let next = PageRequest {
        offset: u64::from(request.limit),
        ..request.clone()
    };
    let next = pool.fetch_page(Reading::UNMARKED, &next).await.unwrap();
    let last = &page.rows.last().expect("a non-empty first page")[at];
    let first = &next.rows.first().expect("a second page")[at];
    assert!(
        in_order(last, first),
        "the next page restarted the ordering: {last:?} then {first:?}"
    );
    (page, next)
}

/// Two cells of one sort column, compared as the server orders them.
fn compare(a: &Value, b: &Value) -> std::cmp::Ordering {
    match (a, b) {
        (Value::Integer(a), Value::Integer(b)) => a.cmp(b),
        // Every text column these fixtures sort on is a timestamp in one
        // zone, which orders as its text does.
        (Value::Text(a), Value::Text(b)) => a.cmp(b),
        other => panic!("cannot compare {other:?}"),
    }
}

/// The three staged edits [`assert_rows_edit_through_key`] makes, each
/// addressed by the table's key.
pub struct KeyedEdits<'a> {
    /// An existing row, whose `note` is updated.
    pub update: Vec<Value>,
    /// A new row: its columns and values.
    pub insert: (&'a [&'a str], Vec<Value>),
    /// The row just inserted, which is deleted again.
    pub delete: Vec<Value>,
}

/// `table` is identified by its primary key `key`, is writable, and an
/// update, an insert and a delete staged through that key each touch exactly
/// one row.
pub async fn assert_rows_edit_through_key(
    pool: &DbPool,
    table: &TableMeta,
    key: &[&str],
    edits: KeyedEdits<'_>,
) {
    let identity = detect_row_identity(table, pool.dialect()).expect("a primary key");
    assert_eq!(
        identity,
        RowIdentity::PrimaryKey {
            columns: key.iter().map(|c| c.to_string()).collect()
        }
    );
    let access = pool.backend_access(table);
    assert!(access.can_mutate(), "{} should be writable", table.name);

    let apply = |change: StagedChange| {
        let (access, identity) = (&access, &identity);
        async move {
            apply_staged(pool, access, table, identity, &[change])
                .await
                .unwrap()
        }
    };
    let counts = apply(StagedChange::Update {
        locator: RowLocator {
            identity_values: edits.update,
        },
        column: "note".into(),
        value: Value::Text("checked".into()),
    })
    .await;
    assert_eq!(counts.updated_rows, 1, "update through the key");

    let (columns, values) = edits.insert;
    let counts = apply(StagedChange::Insert {
        columns: columns.iter().map(|c| c.to_string()).collect(),
        values,
    })
    .await;
    assert_eq!(counts.inserted_rows, 1, "insert");

    let counts = apply(StagedChange::Delete {
        locator: RowLocator {
            identity_values: edits.delete,
        },
    })
    .await;
    assert_eq!(counts.deleted_rows, 1, "delete through the key");
}

/// Every object in a schema `reserved` picks is marked as the engine's own
/// ([`Internal::System`]), there are more than `at_least` of them, and
/// nothing in `public` is marked. Returns the reserved objects, for the
/// checks only one engine makes.
///
/// `reserved` names the schemas independently of the rule under test (the
/// backends classify by `table_type`, `mz_schemas.id` and the like), so a
/// rule that quietly stopped matching fails here rather than selecting
/// nothing and passing.
pub fn assert_schema_is_internal(
    tables: &[TableMeta],
    reserved: impl Fn(&str) -> bool,
    at_least: usize,
) -> Vec<&TableMeta> {
    let engine: Vec<&TableMeta> = tables
        .iter()
        .filter(|t| t.schema.as_deref().is_some_and(&reserved))
        .collect();
    assert!(
        engine.len() > at_least,
        "expected the engine's catalog schemas to be populated, got {}",
        engine.len()
    );
    for table in &engine {
        assert_eq!(table.internal, Some(Internal::System), "{table:?}");
    }
    // ...and the user's own schema is untouched by the rule.
    for table in tables
        .iter()
        .filter(|t| t.schema.as_deref() == Some("public"))
    {
        assert_eq!(table.internal, None, "{table:?}");
    }
    engine
}

/// What an engine whose schema changes escape a script's rollback must show.
pub struct DdlEscape {
    /// Prefixes the two tables, `{prefix}script_dml` and `{prefix}script_ddl`.
    pub prefix: &'static str,
    /// Held around the schema changes, for an engine that refuses concurrent
    /// DDL.
    pub ddl_lock: Option<&'static tokio::sync::Mutex<()>>,
    /// How many of the script's inserted rows the failed script leaves, and
    /// why: the engine's own reason, which is where the engines differ.
    pub rows_left: (i64, &'static str),
    /// Why the table the script created survives the rollback.
    pub table_left: &'static str,
}

/// The engine declares `transactional_ddl: false` and nothing else short of
/// [`Capabilities::FULL`], and a script that inserts a row, creates a table
/// and then fails reports [`Rollback::ExceptSchemaChanges`]: the created table
/// survives, and the inserted row is left as `case.rows_left` says.
///
/// The declaration is pinned separately from the behaviour. A rollback here
/// covers DML and nothing else, so `transactional_ddl` is the one flag that
/// differs from a full-featured engine; declaring `transactions: false`
/// instead would be the worse lie, since rows really do roll back on at least
/// one of these engines.
pub async fn assert_script_ddl_escapes_rollback(pool: &DbPool, case: DdlEscape) {
    assert_eq!(
        pool.backend_capabilities(),
        Capabilities {
            transactional_ddl: false,
            ..Capabilities::FULL
        }
    );
    let (dml, ddl) = (
        format!("{}script_dml", case.prefix),
        format!("{}script_ddl", case.prefix),
    );
    let lock = || async {
        match case.ddl_lock {
            Some(lock) => Some(lock.lock().await),
            None => None,
        }
    };
    {
        let _guard = lock().await;
        for sql in [
            format!("DROP TABLE IF EXISTS {ddl} CASCADE"),
            format!("DROP TABLE IF EXISTS {dml} CASCADE"),
            format!("CREATE TABLE {dml} (id int PRIMARY KEY)"),
        ] {
            pool.query_unbounded(&sql).await.unwrap();
        }
    }

    let sql = format!(
        "INSERT INTO {dml} VALUES (1); \
         CREATE TABLE {ddl} (id int PRIMARY KEY); \
         SELECT * FROM missing_relation"
    );
    let statements = split_statements(&sql, pool.dialect());
    let error = {
        let _guard = lock().await;
        run_script(pool, pool.backend_capabilities(), &statements, |_| {})
            .await
            .expect_err("the final statement names no relation")
    };
    assert_eq!(
        error.rollback,
        Rollback::ExceptSchemaChanges,
        "the rollback did not reach the schema change, and hubro must say so"
    );

    let rows = pool
        .query_unbounded(&format!("SELECT count(*) FROM {dml}"))
        .await
        .unwrap();
    let (expected, why) = case.rows_left;
    assert_eq!(integer(&rows.rows[0][0]), expected, "{why}");

    let survivors = pool
        .query_unbounded(&format!(
            "SELECT count(*) FROM information_schema.tables WHERE table_name = '{ddl}'"
        ))
        .await
        .unwrap();
    assert_eq!(integer(&survivors.rows[0][0]), 1, "{}", case.table_left);

    let _guard = lock().await;
    for sql in [
        format!("DROP TABLE IF EXISTS {ddl} CASCADE"),
        format!("DROP TABLE {dml} CASCADE"),
    ] {
        pool.query_unbounded(&sql).await.unwrap();
    }
}
