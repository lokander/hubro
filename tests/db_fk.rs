//! Integration tests for foreign-key navigation: introspect a
//! parent/child schema, build the FK-jump filter from a real child row with
//! [`build_fk_filter`], and run it through `fetch_page` to prove it selects
//! exactly the referenced parent row(s). Covers a single-column FK, an
//! implicit-PK FK, and a two-column FK, on SQLite and (when configured)
//! Postgres.

mod common;

use hubro::db::Reading;
use std::collections::HashMap;

use common::FixtureDb;
use hubro::db::{
    build_fk_filter, DbPool, ForeignKeyMeta, PageRequest, QueryResult, TableMeta, Value,
};

/// Parent/child schema with a single-column FK (`category_id`), an
/// implicit-PK FK (`alt_category_id REFERENCES category`), and a two-column FK
/// (`region, slot`). Row 10 has every FK populated; row 11 is all-NULL.
const SQLITE_SCHEMA: &str = r#"
    CREATE TABLE category (id INTEGER PRIMARY KEY, name TEXT NOT NULL);
    CREATE TABLE parent (
        region TEXT NOT NULL,
        slot INTEGER NOT NULL,
        label TEXT,
        PRIMARY KEY (region, slot)
    );
    CREATE TABLE child (
        id INTEGER PRIMARY KEY,
        category_id INTEGER REFERENCES category(id),
        alt_category_id INTEGER REFERENCES category,
        region TEXT,
        slot INTEGER,
        FOREIGN KEY (region, slot) REFERENCES parent (region, slot)
    );
    INSERT INTO category (id, name) VALUES (1, 'Books'), (2, 'Music');
    INSERT INTO parent (region, slot, label) VALUES
        ('eu', 1, 'Shelf A'),
        ('us', 2, 'Shelf B');
    INSERT INTO child (id, category_id, alt_category_id, region, slot) VALUES
        (10, 2, 1, 'eu', 1),
        (11, NULL, NULL, NULL, NULL);
"#;

/// The statements building the same schema on Postgres (explicit integer PKs
/// for deterministic ids; `public` schema).
fn postgres_setup() -> Vec<&'static str> {
    vec![
        "DROP TABLE IF EXISTS child",
        "DROP TABLE IF EXISTS parent",
        "DROP TABLE IF EXISTS category",
        "CREATE TABLE category (id integer PRIMARY KEY, name text NOT NULL)",
        "CREATE TABLE parent (
            region text NOT NULL,
            slot integer NOT NULL,
            label text,
            PRIMARY KEY (region, slot)
        )",
        "CREATE TABLE child (
            id integer PRIMARY KEY,
            category_id integer REFERENCES category(id),
            alt_category_id integer REFERENCES category,
            region text,
            slot integer,
            FOREIGN KEY (region, slot) REFERENCES parent (region, slot)
        )",
        "INSERT INTO category (id, name) VALUES (1, 'Books'), (2, 'Music')",
        "INSERT INTO parent (region, slot, label) VALUES ('eu', 1, 'Shelf A'), ('us', 2, 'Shelf B')",
        "INSERT INTO child (id, category_id, alt_category_id, region, slot) VALUES
            (10, 2, 1, 'eu', 1),
            (11, NULL, NULL, NULL, NULL)",
    ]
}

/// Column name → value for one fetched row.
fn row_map(result: &QueryResult, row: &[Value]) -> HashMap<String, Value> {
    result
        .columns
        .iter()
        .zip(row)
        .map(|(col, value)| (col.name.clone(), value.clone()))
        .collect()
}

/// The child row whose `id` column equals `id`, as a column → value map.
async fn child_row(pool: &DbPool, id: i64) -> HashMap<String, Value> {
    let req = common::page("child");
    let page = pool.fetch_page(Reading::UNMARKED, &req).await.unwrap();
    let idx = page
        .columns
        .iter()
        .position(|c| c.name == "id")
        .expect("child has an id column");
    let row = page
        .rows
        .iter()
        .find(|row| row[idx] == Value::Integer(id))
        .unwrap_or_else(|| panic!("no child row with id {id}"));
    row_map(&page, row)
}

/// Finds `child`'s FK whose referencing columns match `columns`.
fn child_fk<'a>(child: &'a TableMeta, columns: &[&str]) -> &'a ForeignKeyMeta {
    child
        .foreign_keys
        .iter()
        .find(|fk| fk.columns == columns)
        .unwrap_or_else(|| panic!("child has no FK on {columns:?}"))
}

/// Runs one FK jump: build the filter from child row `source`, page the target
/// table, and hand the rows back for assertions.
async fn jump(
    pool: &DbPool,
    tables: &[TableMeta],
    fk: &ForeignKeyMeta,
    source: &HashMap<String, Value>,
) -> Vec<Vec<Value>> {
    let target = tables
        .iter()
        .find(|t| t.name == fk.referenced_table && t.schema == fk.referenced_schema)
        .expect("target table is introspected");
    let filter = build_fk_filter(fk, source, target, pool.dialect())
        .expect("a fully-populated FK builds a jump filter");
    let req = PageRequest {
        schema: target.schema.clone(),
        filter: Some(filter),
        ..common::page(target.name.clone())
    };
    let (page, count) = (
        pool.fetch_page(Reading::UNMARKED, &req).await.unwrap(),
        pool.count_rows(Reading::UNMARKED, &req).await.unwrap(),
    );
    // The jump must select exactly the referenced row(s).
    assert_eq!(count, page.rows.len() as u64);
    page.rows
}

/// The shared assertions, run against a pool whose parent/child schema is
/// already set up and seeded.
async fn assert_fk_navigation(pool: &DbPool) {
    let tables = pool.introspect().await.unwrap();
    let child = tables
        .iter()
        .find(|t| t.name == "child")
        .expect("child introspected");
    let row10 = child_row(pool, 10).await;
    let row11 = child_row(pool, 11).await;

    // Single-column FK: category_id = 2 → the 'Music' category row.
    let rows = jump(pool, &tables, child_fk(child, &["category_id"]), &row10).await;
    assert_eq!(rows.len(), 1);
    let category = tables.iter().find(|t| t.name == "category").unwrap();
    let name_idx = category
        .columns
        .iter()
        .position(|c| c.name == "name")
        .unwrap();
    assert_eq!(rows[0][name_idx], Value::Text("Music".into()));

    // Implicit-PK FK: alt_category_id = 1 → the 'Books' category row. On
    // SQLite this exercises the None → target-PK resolution; on Postgres the
    // catalog already records the referenced column.
    let rows = jump(pool, &tables, child_fk(child, &["alt_category_id"]), &row10).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][name_idx], Value::Text("Books".into()));

    // Two-column FK: (region, slot) = ('eu', 1) → the 'Shelf A' parent row,
    // and only it (not 'Shelf B' at ('us', 2)).
    let rows = jump(pool, &tables, child_fk(child, &["region", "slot"]), &row10).await;
    assert_eq!(rows.len(), 1);
    let parent = tables.iter().find(|t| t.name == "parent").unwrap();
    let label_idx = parent
        .columns
        .iter()
        .position(|c| c.name == "label")
        .unwrap();
    assert_eq!(rows[0][label_idx], Value::Text("Shelf A".into()));

    // An all-NULL child row references nothing: no jump is built.
    assert!(build_fk_filter(
        child_fk(child, &["category_id"]),
        &row11,
        category,
        pool.dialect()
    )
    .is_none());
    assert!(
        build_fk_filter(
            child_fk(child, &["region", "slot"]),
            &row11,
            parent,
            pool.dialect()
        )
        .is_none(),
        "a partial/whole NULL composite FK builds no jump"
    );
}

#[tokio::test]
async fn sqlite_fk_jump_selects_the_referenced_rows() {
    let fixture = FixtureDb::with_sql(SQLITE_SCHEMA).await;
    let pool = fixture.open().await;
    assert_fk_navigation(&pool).await;
    pool.close().await;
}

#[tokio::test]
async fn postgres_fk_jump_selects_the_referenced_rows() {
    let Some(url) = common::pg_test_url().await else {
        eprintln!("skipping postgres FK test: HUBRO_PG_TEST_URL not set");
        return;
    };
    let pool = DbPool::open_postgres(&url).await.unwrap();
    for sql in postgres_setup() {
        pool.query_unbounded(sql).await.unwrap();
    }
    assert_fk_navigation(&pool).await;
    pool.close().await;
}

#[tokio::test]
async fn sqlite_references_resolve_whatever_case_they_were_written_in() {
    // SQLite resolves `REFERENCES Parent(ID)` against `parent(id)`
    // case-insensitively, but reports the reference as written; the jump
    // looked for a table named `Parent`, found none and led nowhere. The
    // implicit-key form (`REFERENCES PARENT`) never jumped at all.
    let fixture = FixtureDb::with_sql(
        "CREATE TABLE parent (id INTEGER PRIMARY KEY, body TEXT);
         INSERT INTO parent VALUES (1, '0:'), (2, '1:');
         CREATE TABLE explicit_ref (pid INTEGER REFERENCES Parent(ID), expect TEXT);
         INSERT INTO explicit_ref VALUES (1, '0:'), (2, '1:');
         CREATE TABLE implicit_ref (pid INTEGER REFERENCES PARENT, expect TEXT);
         INSERT INTO implicit_ref VALUES (2, '1:');",
    )
    .await;
    let pool = fixture.open().await;
    let tables = pool.introspect().await.unwrap();
    let named = |name: &str| tables.iter().find(|t| t.name == name).unwrap();
    let parent = named("parent");
    let mut failures = Vec::new();
    for child in ["explicit_ref", "implicit_ref"] {
        let fk = &named(child).foreign_keys[0];
        assert_eq!(fk.referenced_table, "parent", "{child}");
        assert!(
            fk.referenced_columns
                .iter()
                .all(|c| c.as_deref().is_none_or(|c| c == "id")),
            "{child}: {fk:?}"
        );
        failures.extend(common::fk_jump_failures(&pool, named(child), parent).await);
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
    pool.close().await;
}
