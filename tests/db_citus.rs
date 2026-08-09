//! Citus verification (FRE-89). Citus is a sharding extension on stock
//! PostgreSQL, so the Postgres backend drives it unchanged — these tests pin
//! the parts that are *not* stock: distributed tables (one logical table
//! spread over many shard tables), reference tables, and the constraints
//! Citus puts on writes to a distributed table.
//!
//! Needs a running server (Docker only, per CLAUDE.md) and is skipped unless
//! `HUBRO_CITUS_TEST_URL` is set, e.g.:
//!
//! ```sh
//! docker run -d --name hubro-citus-test -p 5435:5432 \
//!   -e POSTGRES_PASSWORD=hubro citusdata/citus:latest
//! docker exec hubro-citus-test psql -U postgres -c 'CREATE DATABASE demo'
//! docker exec hubro-citus-test psql -U postgres -d demo \
//!   -c 'CREATE EXTENSION citus' \
//!   -c "SELECT citus_set_coordinator_host('localhost', 5432)" \
//!   -c "SELECT citus_set_node_property('localhost', 5432, 'shouldhaveshards', true)"
//! HUBRO_CITUS_TEST_URL='postgres://postgres:hubro@localhost:5435/demo?sslmode=disable' cargo test
//! ```
//!
//! Two things about that setup are load-bearing:
//!
//! * The coordinator has to be allowed to hold shards, or
//!   `create_distributed_table` fails with "replication_factor (1) exceeds
//!   number of worker nodes (0)" on a single-node cluster.
//! * `sslmode=disable` is required because the image generates an **X.509 v1**
//!   self-signed certificate on first boot, which rustls refuses to parse — so
//!   every other `sslmode`, `prefer` included, cannot connect at all. That is
//!   the server's certificate being obsolete rather than anything Citus-
//!   specific, and `v1_certificate_failure_says_what_to_do` below pins the
//!   message hubro gives for it.
//!
//! The rest of the Postgres suite also passes against this container — point
//! `HUBRO_PG_TEST_URL` at it to re-run it there.

use std::collections::HashMap;

use tokio::sync::Mutex;

use hubro::db::{
    apply_staged, build_fk_filter, detect_row_identity, DbPool, ExportFormat, Filter, Internal,
    PageRequest, RowIdentity, RowLocator, SortDir, StagedChange, TableKind, TableMeta, Value,
};

fn test_url() -> Option<String> {
    match std::env::var("HUBRO_CITUS_TEST_URL") {
        Ok(url) => Some(url),
        Err(_) => {
            eprintln!("skipping citus test: HUBRO_CITUS_TEST_URL not set");
            None
        }
    }
}

/// Serializes fixture DDL across the tests in this binary.
///
/// Distinct table names are *not* isolation under Citus: `create_distributed_table`
/// and `DROP TABLE` take cluster-wide locks on the `pg_dist_*` catalogs, and
/// every reference table shares one colocation group. Run in parallel, the
/// fixtures deadlock each other — "canceling the transaction since it was
/// involved in a distributed deadlock" — on most runs but not all, which is
/// the worst kind of flake. Ordinary Postgres has no equivalent, which is why
/// no other test file in this repo needs this.
static FIXTURE_DDL: Mutex<()> = Mutex::const_new(());

/// A distributed table sharded on `id`, a reference table it points at, and a
/// plain local table — the three shapes Citus distinguishes. Suffixed per
/// test so each test reads and writes its own rows; the DDL that creates them
/// is serialized by [`FIXTURE_DDL`].
async fn fresh_cluster(pool: &DbPool, suffix: &str) -> (String, String, String) {
    let _guard = FIXTURE_DDL.lock().await;
    let orders = format!("orders_{suffix}");
    let countries = format!("countries_{suffix}");
    let notes = format!("notes_{suffix}");
    for sql in [
        format!("DROP TABLE IF EXISTS {orders} CASCADE"),
        format!("DROP TABLE IF EXISTS {countries} CASCADE"),
        format!("DROP TABLE IF EXISTS {notes} CASCADE"),
        format!("CREATE TABLE {countries} (code text PRIMARY KEY, name text NOT NULL)"),
        format!("SELECT create_reference_table('{countries}')"),
        format!("INSERT INTO {countries} VALUES ('no','Norway'),('se','Sweden'),('dk','Denmark')"),
        format!(
            "CREATE TABLE {orders} (
                id      bigint NOT NULL,
                country text NOT NULL REFERENCES {countries}(code),
                total   numeric(10,2),
                note    text,
                PRIMARY KEY (id)
            )"
        ),
        format!("SELECT create_distributed_table('{orders}', 'id')"),
        format!(
            "INSERT INTO {orders} (id, country, total, note)
             SELECT g, (ARRAY['no','se','dk'])[1 + g % 3], (g * 7.5)::numeric(10,2),
                    CASE WHEN g % 10 = 0 THEN 'flagged' ELSE NULL END
             FROM generate_series(1, 200) g"
        ),
        format!("CREATE TABLE {notes} (id serial PRIMARY KEY, body text)"),
        format!("INSERT INTO {notes} (body) VALUES ('alpha'),('beta')"),
    ] {
        pool.query(&sql).await.unwrap();
    }
    (orders, countries, notes)
}

fn find<'a>(tables: &'a [TableMeta], name: &str) -> &'a TableMeta {
    tables
        .iter()
        .find(|t| t.name == name && t.schema.as_deref() == Some("public"))
        .unwrap_or_else(|| panic!("{name} missing from introspection"))
}

#[tokio::test]
async fn citus_hides_its_own_shard_tables_from_the_catalog() {
    let Some(url) = test_url() else { return };
    let pool = DbPool::open_postgres(&url).await.unwrap();
    let (orders, _, _) = fresh_cluster(&pool, "shards").await;

    // The shards are real tables in `public` — this is a single-node cluster,
    // so they live right beside the table they shard.
    let shards = pool
        .query(&format!(
            "SELECT count(*) FROM pg_dist_shard WHERE logicalrelid = '{orders}'::regclass"
        ))
        .await
        .unwrap();
    let shard_count = match &shards.rows[0][0] {
        Value::Integer(n) => *n,
        other => panic!("expected a count, got {other:?}"),
    };
    assert!(shard_count >= 8, "expected many shards, got {shard_count}");

    // Citus keeps them out of `pg_class` for client queries itself
    // (`citus.override_table_visibility`, on by default), so introspection
    // never sees them and hubro needs no rule of its own. Worth pinning: if a
    // future Citus changed that default, the schema tree would fill with
    // `orders_102008` and this test is what would say so.
    let tables = pool.introspect().await.unwrap();
    let leaked: Vec<&str> = tables
        .iter()
        .filter(|t| t.name.starts_with(&format!("{orders}_")))
        .map(|t| t.name.as_str())
        .collect();
    assert!(
        leaked.is_empty(),
        "shard tables leaked into the tree: {leaked:?}"
    );
}

#[tokio::test]
async fn citus_own_views_in_public_are_marked_internal() {
    let Some(url) = test_url() else { return };
    let pool = DbPool::open_postgres(&url).await.unwrap();

    // Citus installs `citus_tables`/`citus_schemas` into `public` rather than
    // a schema of its own. This is exactly the case a schema-level rule alone
    // would miss, and why FRE-88 attributes objects as well as schemas.
    let tables = pool.introspect().await.unwrap();
    for name in ["citus_tables", "citus_schemas"] {
        assert_eq!(
            find(&tables, name).internal,
            Some(Internal::Extension("citus".into())),
            "{name} should be attributed to the citus extension"
        );
    }

    // And the user's own tables in that same schema stay the user's.
    let (orders, _, notes) = fresh_cluster(&pool, "views").await;
    let tables = pool.introspect().await.unwrap();
    assert_eq!(find(&tables, &orders).internal, None);
    assert_eq!(find(&tables, &notes).internal, None);
}

#[tokio::test]
async fn distributed_reference_and_local_tables_all_introspect_alike() {
    let Some(url) = test_url() else { return };
    let pool = DbPool::open_postgres(&url).await.unwrap();
    let (orders, countries, notes) = fresh_cluster(&pool, "intro").await;

    let tables = pool.introspect().await.unwrap();

    // Citus's three table shapes are all ordinary tables to the catalog, and
    // hubro reports them that way — nothing about distribution is visible in
    // the metadata model, which is what makes the backend work unchanged.
    for name in [&orders, &countries, &notes] {
        let table = find(&tables, name);
        assert_eq!(table.kind, TableKind::Table, "{name}");
        assert_eq!(table.internal, None, "{name}");
        assert!(
            detect_row_identity(table, pool.dialect()).is_some(),
            "{name} should have a row identity"
        );
    }

    // The FK from the distributed table out to the reference table survives.
    let orders_meta = find(&tables, &orders);
    let fk = orders_meta
        .foreign_keys
        .first()
        .expect("fk to the reference table");
    assert_eq!(fk.columns, ["country"]);
    assert_eq!(fk.referenced_table, countries);
}

#[tokio::test]
async fn distributed_table_pages_sorts_and_filters_across_shards() {
    let Some(url) = test_url() else { return };
    let pool = DbPool::open_postgres(&url).await.unwrap();
    let (orders, _, _) = fresh_cluster(&pool, "page").await;

    // A filter that matches rows on many shards: Citus has to fan the query
    // out and merge, and the ordering has to survive the merge.
    let request = PageRequest {
        schema: Some("public".into()),
        table: orders.clone(),
        limit: 5,
        offset: 0,
        sort: Some(("id".into(), SortDir::Desc)),
        filter: Some(Filter::equals("country", "no")),
        extra_key_column: None,
    };
    let total = pool.count_rows(&request).await.unwrap();
    assert!(total > 50, "expected a spread of rows, got {total}");

    let page = pool.fetch_page(&request).await.unwrap();
    assert_eq!(page.rows.len(), 5);
    let ids: Vec<i64> = page
        .rows
        .iter()
        .map(|row| match &row[0] {
            Value::Integer(n) => *n,
            other => panic!("expected an id, got {other:?}"),
        })
        .collect();
    let mut sorted = ids.clone();
    sorted.sort_by(|a, b| b.cmp(a));
    assert_eq!(ids, sorted, "rows should come back in descending id order");

    // The next page continues the same ordering rather than restarting per
    // shard, which is the thing a fan-out merge could plausibly get wrong.
    let next = pool
        .fetch_page(&PageRequest {
            offset: 5,
            ..request
        })
        .await
        .unwrap();
    let next_first = match &next.rows[0][0] {
        Value::Integer(n) => *n,
        other => panic!("expected an id, got {other:?}"),
    };
    assert!(next_first < ids[4]);
}

#[tokio::test]
async fn distributed_rows_edit_through_the_distribution_key() {
    let Some(url) = test_url() else { return };
    let pool = DbPool::open_postgres(&url).await.unwrap();
    let (orders, _, _) = fresh_cluster(&pool, "edit").await;

    let tables = pool.introspect().await.unwrap();
    let table = find(&tables, &orders);
    let identity = detect_row_identity(table, pool.dialect()).expect("pk");
    // Citus requires the distribution column in the WHERE clause of an
    // UPDATE/DELETE. Here the PK *is* the distribution column, so the locator
    // hubro builds from row identity already satisfies that — no Citus-aware
    // code needed on the write path.
    assert_eq!(
        identity,
        RowIdentity::PrimaryKey {
            columns: vec!["id".into()]
        }
    );

    let access = pool.backend_access(table);
    let counts = apply_staged(
        &pool,
        &access,
        table,
        &identity,
        &[StagedChange::Update {
            locator: RowLocator {
                identity_values: vec![Value::Integer(7)],
            },
            column: "note".into(),
            value: Value::Text("checked".into()),
        }],
    )
    .await
    .unwrap();
    assert_eq!(counts.updated_rows, 1);

    let counts = apply_staged(
        &pool,
        &access,
        table,
        &identity,
        &[StagedChange::Insert {
            columns: vec!["id".into(), "country".into(), "total".into()],
            values: vec![
                Value::Integer(9001),
                Value::Text("no".into()),
                Value::Text("12.50".into()),
            ],
        }],
    )
    .await
    .unwrap();
    assert_eq!(counts.inserted_rows, 1);

    let counts = apply_staged(
        &pool,
        &access,
        table,
        &identity,
        &[StagedChange::Delete {
            locator: RowLocator {
                identity_values: vec![Value::Integer(9001)],
            },
        }],
    )
    .await
    .unwrap();
    assert_eq!(counts.deleted_rows, 1);
}

#[tokio::test]
async fn changing_the_distribution_column_is_refused_with_citus_own_message() {
    let Some(url) = test_url() else { return };
    let pool = DbPool::open_postgres(&url).await.unwrap();
    let (orders, _, _) = fresh_cluster(&pool, "distcol").await;

    let tables = pool.introspect().await.unwrap();
    let table = find(&tables, &orders);
    let identity = detect_row_identity(table, pool.dialect()).unwrap();

    // Citus refuses to move a row between shards, so editing the
    // distribution column fails at the server. hubro has no way to know that
    // ahead of time — the constraint is invisible in the catalog metadata —
    // so what matters is that the refusal arrives as a clear error rather
    // than a silent no-op or a partial write.
    let err = apply_staged(
        &pool,
        &pool.backend_access(table),
        table,
        &identity,
        &[StagedChange::Update {
            locator: RowLocator {
                identity_values: vec![Value::Integer(11)],
            },
            column: "id".into(),
            value: Value::Integer(999_111),
        }],
    )
    .await
    .expect_err("citus should refuse to move a row between shards");
    // Citus's own words are "modifying the partition value of rows is not
    // allowed" — it calls the distribution column the partition value.
    let message = format!("{err:?}").to_lowercase();
    assert!(
        message.contains("partition value"),
        "the refusal should carry Citus's reason, got: {message}"
    );
    // And the failure names which staged change died, so a batch of edits
    // points at the offending row rather than failing anonymously.
    assert_eq!(err.change_index, Some(0));
    assert!(err
        .change_summary
        .as_deref()
        .is_some_and(|summary| summary.contains("columns id")));

    // And the row is untouched — the failed statement took nothing with it.
    let page = pool
        .fetch_page(&PageRequest {
            schema: Some("public".into()),
            table: orders.clone(),
            limit: 1,
            offset: 0,
            sort: None,
            filter: Some(Filter::equals("id", "11")),
            extra_key_column: None,
        })
        .await
        .unwrap();
    assert_eq!(page.rows.len(), 1, "the original row should still be there");
}

#[tokio::test]
async fn reference_table_edits_like_an_ordinary_table() {
    let Some(url) = test_url() else { return };
    let pool = DbPool::open_postgres(&url).await.unwrap();
    let (_, countries, _) = fresh_cluster(&pool, "ref").await;

    let tables = pool.introspect().await.unwrap();
    let table = find(&tables, &countries);
    let identity = detect_row_identity(table, pool.dialect()).expect("pk");

    // A reference table is replicated to every node, so a write touches all
    // of them — but it has no distribution column, and the write path is
    // just SQL either way.
    let counts = apply_staged(
        &pool,
        &pool.backend_access(table),
        table,
        &identity,
        &[StagedChange::Insert {
            columns: vec!["code".into(), "name".into()],
            values: vec![Value::Text("fi".into()), Value::Text("Finland".into())],
        }],
    )
    .await
    .unwrap();
    assert_eq!(counts.inserted_rows, 1);

    let counts = apply_staged(
        &pool,
        &pool.backend_access(table),
        table,
        &identity,
        &[StagedChange::Delete {
            locator: RowLocator {
                identity_values: vec![Value::Text("fi".into())],
            },
        }],
    )
    .await
    .unwrap();
    assert_eq!(counts.deleted_rows, 1);
}

#[tokio::test]
async fn fk_navigation_crosses_from_distributed_to_reference() {
    let Some(url) = test_url() else { return };
    let pool = DbPool::open_postgres(&url).await.unwrap();
    let (orders, countries, _) = fresh_cluster(&pool, "fknav").await;

    let tables = pool.introspect().await.unwrap();
    let orders_meta = find(&tables, &orders);
    let fk = orders_meta.foreign_keys.first().unwrap();

    let source_row: HashMap<String, Value> =
        HashMap::from([("country".to_string(), Value::Text("se".into()))]);
    let filter = build_fk_filter(fk, &source_row, &["code".to_string()])
        .expect("a filter pinning the referenced row");

    let page = pool
        .fetch_page(&PageRequest {
            schema: Some("public".into()),
            table: countries.clone(),
            limit: 10,
            offset: 0,
            sort: None,
            filter: Some(filter),
            extra_key_column: None,
        })
        .await
        .unwrap();
    assert_eq!(page.rows.len(), 1);
    assert_eq!(page.rows[0][0], Value::Text("se".into()));
}

#[tokio::test]
async fn v1_certificate_failure_says_what_to_do() {
    let Some(url) = test_url() else { return };

    // The stock Citus image's auto-generated certificate is X.509 v1, which
    // rustls will not parse — so this fails at every sslmode except
    // `disable`, including `prefer`. rustls's own words are
    // "UnsupportedCertVersion", which tells a user nothing; this pins the
    // translation (FRE-89).
    //
    // Skipped rather than failed if the server has a modern certificate, so
    // the suite still passes against a Citus someone has re-certified.
    let Some(base) = url.split('?').next() else {
        return;
    };
    let connected = DbPool::open_postgres(&format!("{base}?sslmode=require")).await;
    let err = match connected {
        Ok(pool) => {
            // Someone re-certified the server, so there is no v1 failure to
            // check. Prove the success is the benign kind before accepting
            // it: a regression that downgraded to plaintext, or one that
            // started accepting certificates it should reject, would also
            // land here and would otherwise skip silently.
            let ssl = pool
                .query("SELECT ssl FROM pg_stat_ssl WHERE pid = pg_backend_pid()")
                .await
                .unwrap();
            // Postgres booleans decode as text here on purpose, so that a
            // viewer shows "true" rather than 1 (see `decode_typed`).
            assert_eq!(
                ssl.rows[0][0],
                Value::Text("true".into()),
                "sslmode=require connected without TLS"
            );
            eprintln!("skipping: this server has a certificate rustls accepts");
            return;
        }
        Err(err) => err,
    };

    let message = format!("{err}");
    assert!(
        message.contains("X.509 v1"),
        "the error should name the real problem, got: {message}"
    );
    assert!(
        message.contains("sslmode=disable"),
        "the error should offer a way forward, got: {message}"
    );
}

#[tokio::test]
async fn scripts_and_exports_span_distributed_and_reference_tables() {
    let Some(url) = test_url() else { return };
    let pool = DbPool::open_postgres(&url).await.unwrap();
    let (orders, countries, _) = fresh_cluster(&pool, "script").await;

    // The shared Postgres suite covers scripts and exports, but only over
    // plain local tables — pointing it at this container proves nothing about
    // distributed ones. Citus 14 has dropped the classic restrictions on
    // mixing reference and distributed writes in one transaction, so this is
    // a tripwire for them coming back rather than a known-broken case.
    //
    // Through `begin_script_tx`, which is the script tab's real path: it
    // pins one connection for the whole transaction. Issuing BEGIN and COMMIT
    // as separate `pool.query` calls would scatter them across the pool and
    // strand an open transaction holding cluster-wide Citus locks.
    let mut tx = pool.begin_script_tx().await.unwrap();
    tx.execute(&format!("INSERT INTO {countries} VALUES ('is', 'Iceland')"))
        .await
        .unwrap();
    tx.execute(&format!(
        "INSERT INTO {orders} (id, country, total) VALUES (90001, 'is', 1.00)"
    ))
    .await
    .unwrap();
    let updated = tx
        .execute(&format!(
            "UPDATE {orders} SET total = 2.00 WHERE id = 90001"
        ))
        .await
        .unwrap();
    assert_eq!(updated, 1);
    tx.commit().await.unwrap();

    // A second transaction rolls back, and takes both tables' changes with it
    // — the distributed write and the reference write are one unit.
    let mut tx = pool.begin_script_tx().await.unwrap();
    tx.execute(&format!("DELETE FROM {orders} WHERE id = 90001"))
        .await
        .unwrap();
    tx.execute(&format!(
        "INSERT INTO {countries} VALUES ('gl', 'Greenland')"
    ))
    .await
    .unwrap();
    tx.rollback().await;

    let survived = pool
        .query(&format!("SELECT count(*) FROM {orders} WHERE id = 90001"))
        .await
        .unwrap();
    assert_eq!(
        survived.rows[0][0],
        Value::Integer(1),
        "the rolled-back delete should not have stuck"
    );
    let greenland = pool
        .query(&format!(
            "SELECT count(*) FROM {countries} WHERE code = 'gl'"
        ))
        .await
        .unwrap();
    assert_eq!(greenland.rows[0][0], Value::Integer(0));

    // Export reads a distributed table through the same shard fan-out.
    let mut csv = Vec::new();
    let exported = pool
        .export(
            &format!("SELECT id, country, total FROM {orders} WHERE country = $1 ORDER BY id"),
            &[Value::Text("no".into())],
            ExportFormat::Csv,
            &mut csv,
        )
        .await
        .unwrap();
    assert!(exported > 50, "expected the filtered rows, got {exported}");
    let text = String::from_utf8(csv).unwrap();
    assert!(text.starts_with("id,country,total"), "{text:.40}");

    pool.query(&format!("DELETE FROM {orders} WHERE id = 90001"))
        .await
        .unwrap();
    pool.query(&format!("DELETE FROM {countries} WHERE code = 'is'"))
        .await
        .unwrap();
}

#[tokio::test]
async fn shards_are_marked_internal_when_citus_stops_hiding_them() {
    let Some(url) = test_url() else { return };
    let pool = DbPool::open_postgres(&url).await.unwrap();

    // `citus.show_shards_for_app_name_prefixes` is a documented knob, and
    // widening it puts every shard table in front of the client: a 26-table
    // database reports 290. Shards are not extension members and not
    // partitions, so none of FRE-88's three rules catch them — hence the
    // `pg_dist_shard` pass this pins.
    //
    // The setting is per-database, so this test needs one of its own rather
    // than changing what the sibling tests see. `DROP ... WITH (FORCE)`
    // clears a leftover from a crashed run.
    let _guard = FIXTURE_DDL.lock().await;
    let db = "hubro_citus_shardvis";
    pool.query(&format!("DROP DATABASE IF EXISTS {db} WITH (FORCE)"))
        .await
        .unwrap();
    pool.query(&format!("CREATE DATABASE {db}")).await.unwrap();
    pool.query(&format!(
        "ALTER DATABASE {db} SET citus.show_shards_for_app_name_prefixes TO '*'"
    ))
    .await
    .unwrap();

    let shard_url = swap_database(&url, db);
    let shard_pool = DbPool::open_postgres(&shard_url).await.unwrap();
    for sql in [
        "CREATE EXTENSION citus",
        "SELECT citus_set_coordinator_host('localhost', 5432)",
        "SELECT citus_set_node_property('localhost', 5432, 'shouldhaveshards', true)",
        "CREATE TABLE visible_shards (id bigint PRIMARY KEY, v text)",
        "SELECT create_distributed_table('visible_shards', 'id')",
    ] {
        shard_pool.query(sql).await.unwrap();
    }

    let tables = shard_pool.introspect().await.unwrap();
    let shards: Vec<&TableMeta> = tables
        .iter()
        .filter(|t| t.name.starts_with("visible_shards_"))
        .collect();
    assert!(
        shards.len() >= 8,
        "expected the shards to be visible for this test to mean anything, got {}",
        shards.len()
    );
    for shard in &shards {
        assert_eq!(
            shard.internal,
            Some(Internal::Extension("citus".into())),
            "shard {} should be hidden by default",
            shard.name
        );
    }
    // The table they shard is the user's, and stays visible.
    assert_eq!(find(&tables, "visible_shards").internal, None);

    shard_pool.close().await;
    pool.query(&format!("DROP DATABASE {db} WITH (FORCE)"))
        .await
        .unwrap();
}

/// Rewrites a Postgres URL to point at a different database, preserving the
/// query string (this suite's URLs carry `sslmode=disable`).
fn swap_database(url: &str, database: &str) -> String {
    let (base, query) = match url.split_once('?') {
        Some((base, query)) => (base, Some(query)),
        None => (url, None),
    };
    let root = base.rsplit_once('/').expect("a url with a database path").0;
    match query {
        Some(query) => format!("{root}/{database}?{query}"),
        None => format!("{root}/{database}"),
    }
}
