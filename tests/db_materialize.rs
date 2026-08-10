//! Materialize verification (FRE-92). Materialize speaks the Postgres wire
//! protocol over a streaming engine: its objects are sources, views and
//! materialized views maintained by the engine rather than tables written by
//! hand. It has the slimmest catalog of any engine in this milestone, which is
//! what makes it the useful test — of the Postgres path's tolerance, and of the
//! capability model (FRE-87).
//!
//! Needs a running server (Docker only, per CLAUDE.md) and is skipped unless
//! `HUBRO_MATERIALIZE_TEST_URL` is set, e.g.:
//!
//! ```sh
//! docker run -d --name hubro-materialize-test -p 6875:6875 \
//!   materialize/materialized:latest
//! HUBRO_MATERIALIZE_TEST_URL='postgres://materialize@localhost:6875/materialize' cargo test
//! ```
//!
//! No password (the stock image trusts every connection) and no TLS.
//!
//! ## What the verification found
//!
//! Introspection failed outright, in three different ways, all now fixed:
//!
//!  1. **The column query asked for columns that do not exist.** Materialize's
//!     `information_schema.columns` has eleven columns to stock Postgres's
//!     forty-four — no `udt_schema`/`udt_name`, and none of the
//!     identity/generated flags. Selecting an absent column fails the whole
//!     statement, so the schema tree died exactly as it did on CockroachDB's
//!     `pk_position` (FRE-90). The query now falls back to a portable shape
//!     that selects the missing pieces as the constants they would decode to.
//!  2. **The index query asked for `indnkeyatts`**, which only exists on
//!     engines that have INCLUDE columns to exclude. Same fallback treatment.
//!  3. **Materialized views arrived twice.** "Matviews are not in
//!     `information_schema`" is a PostgreSQL choice, not a rule — Materialize
//!     lists them there *and* reports `relkind = 'm'`, so both halves of
//!     hubro's UNION claimed them and every matview appeared once whole and
//!     once as an empty duplicate. Both halves now take only what the other
//!     did not, which is a no-op on a server that behaves like stock Postgres.
//!
//! One wart is recorded rather than fixed: Materialize returns the *string*
//! `NULL` for `column_default` where Postgres returns SQL NULL, so every column
//! reads as having a default. Two consumers see it, and it is harmless to both
//! — a column with no default really does default to NULL. Insert
//! required-column detection (FRE-25) treats every column as auto-assigned,
//! which no Materialize object reaches anyway because none are editable; and
//! Show DDL (FRE-108) emits an explicit `DEFAULT NULL` on every column, which
//! is redundant but true and re-runnable.
//!
//! And 265 objects across five reserved schemas would have buried the user's
//! own. Nothing cross-engine reaches them — unlike CockroachDB's they are
//! ordinary tables and views, not `SYSTEM VIEW`, and `pg_depend` is empty — but
//! `mz_schemas` marks them: `database_id` is null exactly for the schemas that
//! belong to no database, which is what a system schema is. That is the case
//! [`PgFlavor`] exists for — the engine's identity says *which* catalog to ask,
//! and the catalog still supplies the answer.
//!
//! (The `pg_depend` path that finds extension objects on other engines finds
//! nothing here. Not because the table is empty — it has rows — but because
//! none of them are `deptype = 'e'`: Materialize has no extensions, and these
//! schemas are the engine itself.)
//!
//! ## What the schema tree deliberately does not show
//!
//! FRE-92 asked how much of Materialize's object model to surface. Clusters,
//! sinks and indexes are first-class there and have no Postgres equivalent, and
//! they stay unrepresented: hubro's tree is relations you can read rows from,
//! and none of the three is one. A sink writes *out* to Kafka, a cluster is
//! compute, an index is already carried as [`TableMeta::indexes`] where it
//! applies. Sources are the exception, and are included, because a source *is*
//! readable — `SELECT` works on it, which is the whole test.
//!
//! ## What the capability model concluded, and why it is not what was expected
//!
//! FRE-92 assumed Materialize would land as a read-only connection. It does
//! not, and declaring it so would have been wrong twice over: `INSERT`,
//! `UPDATE`, `DELETE`, `CREATE TABLE` and real transactional rollback all work
//! here — better than CockroachDB or YugabyteDB, both of which let DDL escape a
//! rolled-back script. So the connection declares [`Capabilities::FULL`], and
//! the SQL editor is right to offer every one of those.
//!
//! Grid editing is nevertheless unavailable on every object, and arrives that
//! way through the model rather than around it:
//!
//! * **Tables** — Materialize rejects `PRIMARY KEY` and `UNIQUE` outright, so
//!   no table has a key and none ever will. Row identity resolves to `None` and
//!   the object narrows to [`Restriction::NoRowIdentity`]. The reason shown is
//!   the true one: there is no way to address one row.
//! * **Views and materialized views** — read-only for the ordinary reasons.
//! * **Sources** — continuously written by the engine from somewhere else, so
//!   they carry a declared restriction saying that, rather than the "Views are
//!   read-only" that their [`TableKind`] would otherwise produce.
//!
//! This is the end-to-end test FRE-87 was built for, with the outcome inverted:
//! the connection is fully capable and every *object* narrows, rather than the
//! connection being clamped shut.

use hubro::db::{
    detect_row_identity, Capabilities, DbPool, Filter, Generated, Internal, PageRequest, PgFlavor,
    Restriction, SortDir, TableKind, TableMeta, TypeDetail, Value,
};

fn test_url() -> Option<String> {
    match std::env::var("HUBRO_MATERIALIZE_TEST_URL") {
        Ok(url) => Some(url),
        Err(_) => {
            eprintln!("skipping materialize test: HUBRO_MATERIALIZE_TEST_URL not set");
            None
        }
    }
}

/// A table plus a view and a materialized view over it. Suffixed per test so
/// the tests in this binary can run concurrently against one database.
async fn fresh_fixture(pool: &DbPool, suffix: &str) -> (String, String, String) {
    let readings = format!("mz_readings_{suffix}");
    let view = format!("mz_warm_{suffix}");
    let matview = format!("mz_avg_{suffix}");
    for sql in [
        format!("DROP MATERIALIZED VIEW IF EXISTS {matview}"),
        format!("DROP VIEW IF EXISTS {view}"),
        format!("DROP TABLE IF EXISTS {readings}"),
        // No PRIMARY KEY: Materialize refuses one. See
        // `materialize_refuses_keys_so_no_table_is_editable`.
        format!("CREATE TABLE {readings} (id int, sensor_id int, temperature double precision)"),
        format!(
            "INSERT INTO {readings} VALUES
                (1, 1, 1.5), (2, 1, 3.0), (3, 1, 4.5), (4, 2, 6.0), (5, 2, 7.5)"
        ),
        format!(
            "CREATE VIEW {view} AS SELECT id, temperature FROM {readings} WHERE temperature > 3"
        ),
        format!(
            "CREATE MATERIALIZED VIEW {matview} AS
             SELECT sensor_id, avg(temperature) AS avg_temp FROM {readings} GROUP BY sensor_id"
        ),
    ] {
        pool.query(&sql).await.unwrap();
    }
    (readings, view, matview)
}

fn find<'a>(tables: &'a [TableMeta], name: &str) -> &'a TableMeta {
    tables
        .iter()
        .find(|t| t.name == name && t.schema.as_deref() == Some("public"))
        .unwrap_or_else(|| panic!("{name} missing from introspection"))
}

fn integer(value: &Value) -> i64 {
    match value {
        Value::Integer(n) => *n,
        other => panic!("expected an integer cell, got {other:?}"),
    }
}

#[tokio::test]
async fn materialize_is_not_mistaken_for_stock_postgres() {
    let Some(url) = test_url() else { return };
    let pool = DbPool::open_postgres(&url).await.unwrap();

    // Materialize's `version()` leads with `PostgreSQL 9.5`, so a detector
    // matching the claimed Postgres version first would file it as stock — and
    // unlike the other two engines here, this one's flavor is load-bearing:
    // the reserved-schema rule below is gated on it.
    assert_eq!(pool.pg_flavor(), Some(PgFlavor::Materialize));

    pool.close().await;
}

#[tokio::test]
async fn materialize_introspects_despite_its_slim_catalog() {
    let Some(url) = test_url() else { return };
    let pool = DbPool::open_postgres(&url).await.unwrap();
    let (readings, view, matview) = fresh_fixture(&pool, "intro").await;

    // The regression guard for findings 1 and 2: with either fallback missing,
    // this call fails and nothing below is reachable.
    let tables = pool.introspect().await.unwrap();

    let table = find(&tables, &readings);
    assert_eq!(table.kind, TableKind::Table);
    assert_eq!(table.internal, None);
    let columns: Vec<&str> = table.columns.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(columns, ["id", "sensor_id", "temperature"]);
    // What the portable column shape gives up, asserted rather than assumed:
    // no identity/generated classification and no enum or array structure.
    // That is the correct answer here rather than a degraded one — Materialize
    // has none of them to report.
    assert!(table
        .columns
        .iter()
        .all(|c| c.generated == Generated::Never && c.type_detail == TypeDetail::Plain));

    // A wart worth recording rather than fixing. Materialize returns the
    // *string* `NULL` for `column_default` where Postgres returns SQL NULL, so
    // every column reads as having a default and therefore as auto-assigned.
    // It is harmless here — a column with no default really does default to
    // NULL, and the only consumer of `is_auto_assigned` is insert
    // required-column detection (FRE-25), which no Materialize object ever
    // reaches because none of them are editable. If an engine ever pairs this
    // spelling with working inserts, this is the assertion that will say so.
    assert!(table
        .columns
        .iter()
        .all(|c| c.default.as_deref() == Some("NULL")));
    assert!(table.columns.iter().all(|c| c.is_auto_assigned()));

    assert_eq!(find(&tables, &view).kind, TableKind::View);
    assert_eq!(find(&tables, &matview).kind, TableKind::MaterializedView);

    pool.close().await;
}

#[tokio::test]
async fn materialize_materialized_view_arrives_exactly_once() {
    let Some(url) = test_url() else { return };
    let pool = DbPool::open_postgres(&url).await.unwrap();
    let (_, _, matview) = fresh_fixture(&pool, "dedup").await;

    let tables = pool.introspect().await.unwrap();

    // The regression guard for finding 3. Materialize reports its materialized
    // views in `information_schema.tables` *and* as `relkind = 'm'`, so before
    // the fix both halves of the UNION claimed each one: the object appeared
    // twice, and the duplicate — reached second — held no columns at all.
    let matches: Vec<&TableMeta> = tables
        .iter()
        .filter(|t| t.name == matview && t.schema.as_deref() == Some("public"))
        .collect();
    assert_eq!(matches.len(), 1, "duplicated: {matches:?}");

    let columns: Vec<&str> = matches[0].columns.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(
        columns,
        ["sensor_id", "avg_temp"],
        "the surviving entry must be the one with columns"
    );

    pool.close().await;
}

#[tokio::test]
async fn materialize_reserved_catalog_schemas_are_marked_as_the_engines_own() {
    let Some(url) = test_url() else { return };
    let pool = DbPool::open_postgres(&url).await.unwrap();
    fresh_fixture(&pool, "reserved").await;

    let tables = pool.introspect().await.unwrap();

    // Selected by schema here even though the backend classifies them through
    // `mz_schemas.id`, so the test names the objects independently of the rule
    // under test — a rule that quietly stopped matching would fail this rather
    // than select nothing and pass.
    let reserved: Vec<&TableMeta> = tables
        .iter()
        .filter(|t| {
            // The fixtures are tables named `mz_*` inside `public`, never
            // schemas, so matching the schema name cannot catch them.
            t.schema.as_deref().is_some_and(|s| s.starts_with("mz_"))
        })
        .collect();
    assert!(
        reserved.len() > 100,
        "expected Materialize's catalog schemas to be populated, got {}",
        reserved.len()
    );
    for table in &reserved {
        assert_eq!(table.internal, Some(Internal::System), "{table:?}");
    }

    // ...and the user's own schema is untouched by the rule.
    for table in tables
        .iter()
        .filter(|t| t.schema.as_deref() == Some("public"))
    {
        assert_eq!(table.internal, None, "{table:?}");
    }

    pool.close().await;
}

#[tokio::test]
async fn materialize_refuses_keys_so_no_table_is_editable() {
    let Some(url) = test_url() else { return };
    let pool = DbPool::open_postgres(&url).await.unwrap();
    let (readings, _, _) = fresh_fixture(&pool, "nokey").await;

    // Not a property of this fixture but of the engine: Materialize rejects
    // both ways of declaring a key, so *no* table here can ever have one.
    let refused = pool
        .query("CREATE TABLE mz_keyed_probe (id int PRIMARY KEY)")
        .await
        .expect_err("Materialize does not support primary keys");
    assert!(
        refused.message().contains("primary key"),
        "expected the engine to say why, got: {refused:?}"
    );

    let tables = pool.introspect().await.unwrap();
    let table = find(&tables, &readings);
    assert!(table.indexes.iter().all(|i| !i.unique));
    assert_eq!(detect_row_identity(table, pool.dialect()), None);

    // So the object narrows for the honest reason: not "this connection is
    // read-only" — it isn't — but "there is no way to address one row".
    let access = pool.backend_access(table);
    assert!(!access.can_mutate());
    assert_eq!(access.restriction, Some(Restriction::NoRowIdentity));

    // Browsing is untouched, which is the half that has to keep working.
    let page = pool
        .fetch_page(&PageRequest {
            schema: Some("public".into()),
            table: readings.clone(),
            limit: 3,
            offset: 0,
            sort: Some(("id".into(), SortDir::Asc)),
            filter: None,
            extra_key_column: None,
        })
        .await
        .unwrap();
    assert_eq!(page.rows.len(), 3);

    pool.close().await;
}

#[tokio::test]
async fn materialize_source_says_the_engine_writes_it() {
    let Some(url) = test_url() else { return };
    let pool = DbPool::open_postgres(&url).await.unwrap();

    let tables = pool.introspect().await.unwrap();

    // Sources are read from `mz_internal` rather than created: `CREATE SOURCE
    // … FROM LOAD GENERATOR` is behind a private-preview feature flag in the
    // stock image, and every other source kind needs an external system to
    // read from. The engine's own sources are the same object kind reported
    // the same way, so they pin the same behaviour.
    let sources: Vec<&TableMeta> = tables
        .iter()
        .filter(|t| t.kind_label.as_deref() == Some("source"))
        .collect();
    assert!(
        !sources.is_empty(),
        "expected Materialize to report source objects"
    );

    for source in &sources {
        // A source is derived and continuously written by the engine, so it is
        // a view's contract rather than a table's — without this it would fall
        // through to `Table` and be offered for editing.
        assert_eq!(source.kind, TableKind::View, "{source:?}");
        assert_eq!(detect_row_identity(source, pool.dialect()), None);

        let access = pool.backend_access(source);
        assert!(!access.can_mutate());
        // ...and the reason names the engine rather than sending the reader
        // looking for a view definition that does not exist.
        let notice = access.read_only_notice().unwrap();
        assert!(
            notice.contains("written by the engine"),
            "expected a source-specific reason, got: {notice}"
        );
    }

    pool.close().await;
}

#[tokio::test]
async fn materialize_views_and_matviews_browse_but_refuse_writes() {
    let Some(url) = test_url() else { return };
    let pool = DbPool::open_postgres(&url).await.unwrap();
    let (_, view, matview) = fresh_fixture(&pool, "readonly").await;

    let tables = pool.introspect().await.unwrap();
    for (name, expected) in [
        (&view, Restriction::View),
        (&matview, Restriction::MaterializedView),
    ] {
        let meta = find(&tables, name);
        let access = pool.backend_access(meta);
        assert!(!access.can_mutate(), "{name} must not be editable");
        assert_eq!(access.restriction, Some(expected), "{name}");
    }

    // Paging, sorting and filtering over a materialized view — the read path
    // that matters most on a streaming engine, since that is what its objects
    // mostly are.
    let request = PageRequest {
        schema: Some("public".into()),
        table: matview.clone(),
        limit: 1,
        offset: 0,
        sort: Some(("sensor_id".into(), SortDir::Asc)),
        filter: Some(Filter::equals("sensor_id", "2")),
        extra_key_column: None,
    };
    assert_eq!(pool.count_rows(&request).await.unwrap(), 1);
    let page = pool.fetch_page(&request).await.unwrap();
    assert_eq!(page.rows.len(), 1);
    assert_eq!(integer(&page.rows[0][0]), 2);

    pool.close().await;
}

#[tokio::test]
async fn materialize_is_a_fully_capable_connection_despite_editing_nothing() {
    let Some(url) = test_url() else { return };
    let pool = DbPool::open_postgres(&url).await.unwrap();

    // The conclusion FRE-92 expected to be the opposite. Every object above is
    // read-only, but the *connection* is not: declaring it read-only would
    // wrongly disable the SQL editor's write paths, which work.
    assert_eq!(pool.backend_capabilities(), Capabilities::FULL);

    let table = "mz_caps_probe";
    pool.query(&format!("DROP TABLE IF EXISTS {table}"))
        .await
        .unwrap();
    pool.query(&format!("CREATE TABLE {table} (id int, note text)"))
        .await
        .unwrap();

    // Each of the three write verbs the capability set promises.
    assert_eq!(
        pool.execute(&format!("INSERT INTO {table} VALUES (1, 'a'), (2, 'b')"))
            .await
            .unwrap(),
        2
    );
    assert_eq!(
        pool.execute(&format!("UPDATE {table} SET note = 'z' WHERE id = 1"))
            .await
            .unwrap(),
        1
    );
    assert_eq!(
        pool.execute(&format!("DELETE FROM {table} WHERE id = 2"))
            .await
            .unwrap(),
        1
    );

    pool.query(&format!("DROP TABLE {table}")).await.unwrap();
    pool.close().await;
}

#[tokio::test]
async fn materialize_rolls_back_a_transaction_including_its_ddl() {
    let Some(url) = test_url() else { return };
    let pool = DbPool::open_postgres(&url).await.unwrap();

    // Worth pinning because the other two engines in this milestone both fail
    // it: on CockroachDB and YugabyteDB a schema change escapes the
    // transaction it was issued in, so a rolled-back script leaves it standing
    // (FRE-146). Materialize does not — which is why that gap has to be a
    // per-engine capability rather than something assumed of Postgres-wire
    // engines generally.
    let table = "mz_tx_probe";
    pool.query(&format!("DROP TABLE IF EXISTS {table}"))
        .await
        .unwrap();

    let mut tx = pool.begin_script_tx().await.unwrap();
    tx.execute(&format!("CREATE TABLE {table} (id int)"))
        .await
        .unwrap();
    tx.rollback().await;

    let survivors = pool
        .query(&format!(
            "SELECT count(*) FROM information_schema.tables WHERE table_name = '{table}'"
        ))
        .await
        .unwrap();
    assert_eq!(
        integer(&survivors.rows[0][0]),
        0,
        "the rolled-back CREATE TABLE must not survive"
    );

    pool.close().await;
}
