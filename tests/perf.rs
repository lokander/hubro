//! Performance fixtures and budgets (FRE-34).
//!
//! This file has two layers:
//!
//! - **Fast, always-on tests** verify the large-fixture generators work at
//!   tiny scale. They run in a plain `cargo test` and build nothing big.
//! - **`#[ignore]`d budget tests** build (or reuse a cached) full-scale
//!   fixture, time a real db-layer operation over several iterations, print a
//!   `PASS/FAIL` report, and assert p50 against a budget so a regression fails.
//!
//! Run the budget suite explicitly (single-threaded so the timings don't
//! contend, and with output shown):
//!
//! ```sh
//! cargo test --test perf -- --ignored --test-threads=1 --nocapture
//! ```
//!
//! The full-scale fixtures are cached under `target/perf-fixtures/`; the first
//! run builds them (tens of seconds; the wide table is ~0.95 GB on disk) and
//! later runs reuse them. Force a rebuild with `HUBRO_PERF_REBUILD=1`, or
//! reclaim the space with `cargo clean` / by deleting that directory.
//!
//! Budgets and their rationale live in `common::budgets`; recorded baseline
//! numbers live in `docs/PERFORMANCE.md`. SQLite is the priority (deterministic,
//! no server needed); an optional Postgres parity check is gated on
//! `HUBRO_PG_TEST_URL` and uses a smaller, un-cached table.

mod common;

use common::{budgets, perf_scale, FixtureDb, Timings};
use hubro::db::{DbPool, PageRequest, Value, PREVIEW_BYTES};

fn request(table: &str) -> PageRequest {
    PageRequest {
        schema: None,
        table: table.into(),
        limit: 100,
        offset: 0,
        sort: None,
        filter: None,
        extra_key_column: None,
    }
}

// ---------------------------------------------------------------------------
// Fast generator sanity checks (run in ordinary `cargo test`)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn wide_generator_builds_a_queryable_table() {
    let fixture = FixtureDb::wide(200, 30).await;
    let pool = fixture.open().await;

    let count = pool.count_rows(&request("wide")).await.unwrap();
    assert_eq!(count, 200);

    let page = pool.fetch_page(&request("wide")).await.unwrap();
    assert_eq!(page.rows.len(), 100);
    assert_eq!(page.columns.len(), 30);

    pool.close().await;
}

#[tokio::test]
async fn big_values_generator_stores_multi_kb_payloads() {
    let fixture = FixtureDb::big_values(3, 8 * 1024).await;
    let pool = fixture.open().await;

    let page = pool.fetch_page(&request("big_values")).await.unwrap();
    assert_eq!(page.rows.len(), 3);
    // payload is the third column; each holds `bytes` bytes.
    if let hubro::db::Value::Text(text) = &page.rows[0][2] {
        assert_eq!(text.len(), 8 * 1024);
    } else {
        panic!("expected TEXT payload");
    }

    pool.close().await;
}

#[tokio::test]
async fn many_tables_generator_introspects_every_table() {
    let fixture = FixtureDb::many_tables(12).await;
    let pool = fixture.open().await;

    let tables = pool.introspect().await.unwrap();
    assert_eq!(tables.len(), 12);
    assert!(tables.iter().all(|t| t.columns.len() == 4));

    pool.close().await;
}

// ---------------------------------------------------------------------------
// Budget tests (ignored; build/reuse full-scale fixtures)
// ---------------------------------------------------------------------------

const ITERS: usize = 7;
// The deep-offset scan is expensive; fewer iterations keep the run bearable.
const DEEP_ITERS: usize = 3;

#[tokio::test]
#[ignore = "builds a 1M-row fixture; run with --ignored"]
async fn budget_wide_table_operations() {
    let fixture = FixtureDb::wide_cached(perf_scale::WIDE_ROWS, perf_scale::WIDE_COLS).await;

    // Connection open (startup proxy).
    let path = fixture.path().to_path_buf();
    Timings::measure("connection_open", ITERS, || {
        let path = path.clone();
        async move {
            let pool = DbPool::open_sqlite(&path).await.unwrap();
            pool.close().await;
        }
    })
    .await
    .report_and_assert(budgets::CONNECTION_OPEN);

    let pool = fixture.open().await;
    let base = request("wide");

    // Time to first rows: count + first page, as the grid does on table open.
    Timings::measure("time_to_first_rows", ITERS, || {
        let pool = &pool;
        let req = base.clone();
        async move {
            let _ = pool.count_rows(&req).await.unwrap();
            let _ = pool.fetch_page(&req).await.unwrap();
        }
    })
    .await
    .report_and_assert(budgets::TIME_TO_FIRST_ROWS);

    Timings::measure("count_rows", ITERS, || {
        let pool = &pool;
        let req = base.clone();
        async move {
            let _ = pool.count_rows(&req).await.unwrap();
        }
    })
    .await
    .report_and_assert(budgets::COUNT_ROWS);

    Timings::measure("page_nav_shallow", ITERS, || {
        let pool = &pool;
        let req = base.clone();
        async move {
            let _ = pool.fetch_page(&req).await.unwrap();
        }
    })
    .await
    .report_and_assert(budgets::PAGE_NAV_SHALLOW);

    let deep = PageRequest {
        offset: perf_scale::WIDE_ROWS - 100,
        ..base.clone()
    };
    Timings::measure("page_nav_deep", DEEP_ITERS, || {
        let pool = &pool;
        let req = deep.clone();
        async move {
            let _ = pool.fetch_page(&req).await.unwrap();
        }
    })
    .await
    .report_and_assert(budgets::PAGE_NAV_DEEP);

    pool.close().await;
}

#[tokio::test]
#[ignore = "builds a 300-table fixture; run with --ignored"]
async fn budget_schema_load() {
    let fixture = FixtureDb::many_tables_cached(perf_scale::SCHEMA_TABLES).await;
    let pool = fixture.open().await;

    Timings::measure("schema_load", ITERS, || {
        let pool = &pool;
        async move {
            let tables = pool.introspect().await.unwrap();
            assert_eq!(tables.len(), perf_scale::SCHEMA_TABLES);
        }
    })
    .await
    .report_and_assert(budgets::SCHEMA_LOAD);

    pool.close().await;
}

#[tokio::test]
#[ignore = "builds a multi-MB-value fixture; run with --ignored"]
async fn budget_big_value_page() {
    let fixture =
        FixtureDb::big_values_cached(perf_scale::BIG_VALUE_ROWS, perf_scale::BIG_VALUE_BYTES).await;
    let pool = fixture.open().await;
    let req = request("big_values");

    Timings::measure("big_value_page", ITERS, || {
        let pool = &pool;
        let req = req.clone();
        async move {
            let page = pool.fetch_page(&req).await.unwrap();
            assert_eq!(page.rows.len() as u64, perf_scale::BIG_VALUE_ROWS);
        }
    })
    .await
    .report_and_assert(budgets::BIG_VALUE_PAGE);

    pool.close().await;
}

/// The bounded page path (FRE-33) must keep peak memory independent of value
/// size. This asserts a **memory bound** — the whole decoded page is far
/// smaller than even one full 4 MB value — as the observable proxy for bounded
/// memory, then times it against the same `big_value_page` budget.
#[tokio::test]
#[ignore = "builds a multi-MB-value fixture; run with --ignored"]
async fn budget_big_value_page_bounded() {
    let fixture =
        FixtureDb::big_values_cached(perf_scale::BIG_VALUE_ROWS, perf_scale::BIG_VALUE_BYTES).await;
    let pool = fixture.open().await;
    let tables = pool.introspect().await.unwrap();
    let cols = tables
        .iter()
        .find(|t| t.name == "big_values")
        .unwrap()
        .columns
        .clone();
    let req = request("big_values");

    // Memory bound: every large cell is capped at PREVIEW_BYTES, so the whole
    // decoded page is a few KB — not rows × 4 MB.
    let page = pool.fetch_page_bounded(&req, &cols, &["id"]).await.unwrap();
    let decoded: usize = page
        .result
        .rows
        .iter()
        .flat_map(|r| r.iter())
        .map(|v| match v {
            Value::Text(t) => t.len(),
            Value::Blob(b) => b.len(),
            _ => 8,
        })
        .sum();
    let bound = page.result.rows.len() * page.result.columns.len() * (PREVIEW_BYTES + 64);
    assert!(
        decoded <= bound,
        "decoded page {decoded} exceeds the bound {bound}"
    );
    assert!(
        decoded < perf_scale::BIG_VALUE_BYTES,
        "the entire page ({decoded} B) must be smaller than one full value \
         ({} B)",
        perf_scale::BIG_VALUE_BYTES
    );

    Timings::measure("big_value_page_bounded", ITERS, || {
        let pool = &pool;
        let req = req.clone();
        let cols = cols.clone();
        async move {
            let page = pool.fetch_page_bounded(&req, &cols, &["id"]).await.unwrap();
            assert_eq!(page.result.rows.len() as u64, perf_scale::BIG_VALUE_ROWS);
        }
    })
    .await
    .report_and_assert(budgets::BIG_VALUE_PAGE);

    pool.close().await;
}

// ---------------------------------------------------------------------------
// Optional Postgres parity check (gated + ignored)
// ---------------------------------------------------------------------------

/// Postgres is parity-only: SQLite is the repeatable, no-dependency budget.
/// This builds a smaller, un-cached table on the server named by
/// `HUBRO_PG_TEST_URL` and reports (does not assert) its timings.
#[tokio::test]
#[ignore = "needs HUBRO_PG_TEST_URL; run with --ignored"]
async fn budget_postgres_parity() {
    let Ok(url) = std::env::var("HUBRO_PG_TEST_URL") else {
        eprintln!("skipping postgres parity: HUBRO_PG_TEST_URL not set");
        return;
    };
    let pool = DbPool::open_postgres(&url).await.unwrap();

    const PG_ROWS: i64 = 100_000;
    pool.query("DROP TABLE IF EXISTS perf_wide").await.unwrap();
    pool.query(
        "CREATE TABLE perf_wide (
            id bigint PRIMARY KEY,
            label text NOT NULL,
            amount double precision
        )",
    )
    .await
    .unwrap();
    pool.query(&format!(
        "INSERT INTO perf_wide (id, label, amount)
         SELECT g, 'row ' || g, g * 0.5 FROM generate_series(1, {PG_ROWS}) AS g"
    ))
    .await
    .unwrap();

    let base = request("perf_wide");
    // Reported for parity; not asserted (server hardware varies).
    let ttfr = Timings::measure("pg_time_to_first_rows", ITERS, || {
        let pool = &pool;
        let req = base.clone();
        async move {
            let _ = pool.count_rows(&req).await.unwrap();
            let _ = pool.fetch_page(&req).await.unwrap();
        }
    })
    .await;
    println!(
        "pg_time_to_first_rows p50={:?} p95={:?}",
        ttfr.p50(),
        ttfr.p95()
    );

    let deep = PageRequest {
        offset: (PG_ROWS - 100) as u64,
        ..base
    };
    let nav = Timings::measure("pg_page_nav_deep", DEEP_ITERS, || {
        let pool = &pool;
        let req = deep.clone();
        async move {
            let _ = pool.fetch_page(&req).await.unwrap();
        }
    })
    .await;
    println!("pg_page_nav_deep p50={:?} p95={:?}", nav.p50(), nav.p95());

    pool.query("DROP TABLE IF EXISTS perf_wide").await.unwrap();
    pool.close().await;
}
