# Performance budgets and baseline (FRE-34)

This document records the performance **budgets** dataview commits to and the
**baseline** measured on the dev machine. The budgets are the contract that
FRE-32 (virtual scrolling) and FRE-33 (bounded memory / streaming) must keep
meeting — they turn "feels fast" into a number a test can fail on.

## How to measure

The harness lives in `tests/perf.rs` with the fixture generators and budget
constants in `tests/common/mod.rs` (`common::budgets`, `common::perf_scale`).

```sh
# Fast generator sanity checks run in the normal suite (build nothing big):
cargo test --test perf

# Full budget suite (builds/reuses the large fixtures, asserts the budgets):
cargo test --test perf -- --ignored --test-threads=1 --nocapture
```

Each budget test warms up once, then times the operation over several
iterations (7; the expensive deep-offset scan uses 3), reports p50/p95, and
asserts **p50 ≤ budget**. `--test-threads=1` keeps concurrent tests from
contending for the timing; `--nocapture` shows the `PASS/FAIL` report lines.

### Fixtures and caching

The large fixtures are generated once and cached under `target/perf-fixtures/`
(removed by `cargo clean`), so repeated runs reuse them:

| Fixture | Scale (`perf_scale`) | On disk |
| --- | --- | --- |
| `wide_1000000x100.db` | 1,000,000 rows × 100 mixed-type columns | ~0.95 GB |
| `schema_300.db` | 300 tables (4 cols + index + rows each) | ~2.5 MB |
| `big_values_4x4194304.db` | 4 rows, each a 4 MiB TEXT **and** 4 MiB BLOB | ~33 MB |

Generation uses `synchronous=OFF` + `journal_mode=MEMORY` and large multi-row
INSERT batches in a single transaction, so the wide table builds in tens of
seconds. Those pragmas apply only to the throwaway generation connection. Force
a rebuild with `DATAVIEW_PERF_REBUILD=1`; reclaim the disk by deleting
`target/perf-fixtures/`.

## Budgets and rationale

Thresholds are p50 targets set generously — roughly 3x the dev-machine baseline
where the operation is cheap enough to regress subtly, looser where the cost is
inherent — so they survive CI hardware (~2-3x slower than the dev box) without
flaking, yet still fail on an order-of-magnitude regression.

| Operation | What it measures | Budget (p50) | Rationale |
| --- | --- | --- | --- |
| `connection_open` | `open_sqlite` on the 1M-row db (app-startup proxy) | 150 ms | Opening a pool touches only the header; anything slower means startup work leaked in. True Dioxus window startup is out of scope for this db-layer harness. |
| `time_to_first_rows` | `count_rows` + `fetch_page(LIMIT 100)` on table open | 900 ms | The user-visible "table opened" latency; dominated by the COUNT(\*) scan. |
| `count_rows` | `COUNT(*)` full scan of 1M rows | 800 ms | The grid's row-count indicator. A regression that decodes every row instead of counting the rowid b-tree blows past this. |
| `page_nav_shallow` | `fetch_page(LIMIT 100, OFFSET 0)` | 100 ms | Normal page-to-page navigation should feel instant. |
| `page_nav_deep` | `fetch_page(LIMIT 100)` near row 1,000,000 | 3000 ms | OFFSET paging is O(offset) in SQLite; this is the loosest budget on purpose and is exactly what keyset pagination (FRE-32) should collapse. |
| `schema_load` | `introspect()` on 300 tables | 1500 ms | The schema sidebar on a large database. |
| `big_value_page` | `fetch_page(LIMIT 100)` over 4× (4 MiB TEXT + 4 MiB BLOB) rows | 500 ms | Fetching fat rows must not stall; the budget for FRE-33 to keep meeting while it bounds memory. |

Postgres is **parity-only**: SQLite is the deterministic, no-dependency budget.
An optional `budget_postgres_parity` test (gated on `DATAVIEW_PG_TEST_URL`,
`#[ignore]`d) builds a smaller un-cached table and *reports* time-to-first-rows
and deep-nav without asserting, since server hardware varies.

## Baseline (dev machine)

- **Date:** 2026-07-11
- **Machine:** AMD Ryzen 9 5950X (16C/32T), Linux, SQLite via sqlx 0.8. Indicative dev-box numbers, not a CI guarantee.
- **Scale:** wide = 1,000,000 × 100; schema = 300 tables; big-values = 4 × 4 MiB.

Measured p50 (p95 in parentheses), `cargo test --test perf -- --ignored`:

| Operation | debug p50 (p95) | release p50 (p95) | Budget |
| --- | --- | --- | --- |
| `connection_open` | 0.34 ms (0.47 ms) | 0.16 ms (0.24 ms) | 150 ms |
| `time_to_first_rows` | 260 ms (263 ms) | 221 ms (225 ms) | 900 ms |
| `count_rows` | 280 ms (284 ms) | 227 ms (231 ms) | 800 ms |
| `page_nav_shallow` | 6.0 ms (6.5 ms) | 1.8 ms (2.5 ms) | 100 ms |
| `page_nav_deep` | 367 ms (369 ms) | 243 ms (244 ms) | 3000 ms |
| `schema_load` | 124 ms (128 ms) | 28 ms (32 ms) | 1500 ms |
| `big_value_page` | 25 ms (28 ms) | 26 ms (38 ms) | 500 ms |

Notes:

- The db-bound operations (`count_rows`, paging) barely differ between debug and
  release — the work is inside SQLite's C core, not the Rust glue. Rust-heavy
  paths (`schema_load` building metadata) are noticeably faster in release.
- `page_nav_deep` at ~a third of a second for a single 100-row page confirms the
  O(offset) cost of OFFSET paging: this is the headline motivation for FRE-32.

## Virtual scrolling (FRE-32)

The budgets above are db-layer numbers. FRE-32 addresses the *other* half of a
"table opened" — how long WebKitGTK spends laying out and painting a page's
worth of rows, and how smoothly it scrolls — which the db harness can't see.

### The problem, measured in the webview

A LIMIT-`PAGE_SIZE` page is only 100 rows, so the row *count* is not the
bottleneck. The cost is **wide** tables: with 120 columns a 100-row page is
`100 × 121 = 12,100` `<td>` nodes, and an auto-layout `<table>` must lay every
one of them out (column widths depend on all cells).

Profiled in the app (debug build, WebKitGTK, `WEBKIT_DISABLE_COMPOSITING_MODE=1`,
a 120-column × 5,000-row SQLite fixture, one 100-row page) by counting rendered
nodes and timing a forced full relayout of `#dv-grid`:

| Grid | DOM `<tr>` | DOM `<td>` | Forced relayout (debug) |
| --- | --- | --- | --- |
| Before (whole page rendered) | 100 | 12,100 | ~5.7 ms |
| After (windowed) | ~28 + 1 spacer | ~3,400 | ~1.5 ms |

The windowed grid keeps only the rows in (and near) the viewport in the DOM, so
node count and layout cost scale with the **viewport**, not the page — ~3.6×
fewer nodes and ~3.8× faster relayout on the 120-column fixture, with more
headroom as columns or (a future larger) page size grow.

An earlier attempt used `content-visibility: auto` on each `<tr>` (a smaller,
less invasive change). Measured in the same webview it made **no difference**
(`rendered=100/100`, relayout ~5.8 ms unchanged): an auto-layout table row can't
skip layout because the table needs every row to size its columns. So we
implemented true windowing instead.

### How it works

- Data rows have a fixed height (`ROW_HEIGHT`, 33 px), so a row's scroll offset
  is `index × ROW_HEIGHT`.
- A scroll/resize listener on `#dv-grid` reports `scrollTop` + `clientHeight`
  (rAF-coalesced, plus a trailing "settle" report so a momentum fling always
  ends with a final update). `compute_visible_range` turns those into the row
  window `[start, end)` — viewport rows plus `ROW_OVERSCAN` (12) on each side,
  with `start` derived backward from a clamped `end` so an overscrolling fling
  can never leave the window empty (a blank viewport).
- The `<tbody>` renders a top spacer `<tr>` of `start × ROW_HEIGHT`, then
  `rows[start..end]`, then a bottom spacer — so the scrollbar and offsets match
  the full page while only the window is in the DOM.
- Every per-row behavior is untouched because rows are still keyed by their
  stable row key and rendered by the same `GridRow`: selection, dirty/edited
  (amber) and pending-delete (red) tints, truncated-preview expand, and FK ↗
  jumps all survive a row scrolling out and back (their state lives in signals
  keyed by row key, re-read when the row re-enters the window). The keyboard
  focus ring (FRE-15) still works: arrowing to an offscreen row scrolls it into
  view (by its computed offset, since row height is fixed), which brings it into
  the window and keeps the ring.
- One edge with inline editing: if an *open* cell editor is scrolled entirely
  out of the window by the mouse wheel, its `<tr>` unmounts before an
  Enter/Tab/blur commit, so uncommitted keystrokes in that cell can be lost
  (scrolling back re-opens it from the stored value). Committing (Enter/Tab) or
  clicking away first is unaffected; this only bites mid-edit wheel-scrolling.

### Paging model

Unchanged, deliberately: the grid still pages at LIMIT `PAGE_SIZE` = 100 with
Prev/Next, and windowing applies **within** the current page. Scrolling across a
page boundary still uses the pager. This is the low-risk choice — it keeps
per-page memory bounded (FRE-33) and leaves the fetch/count path alone. A larger
page or infinite-scroll-within-a-window would be a separate, bigger change.

### In-budget counts

- **Columns:** smooth (relayout ~1.5 ms, no scroll jank) at **120 columns × a
  100-row page** — comfortably within a 60 Hz (16.7 ms) frame budget in a debug
  build. Column virtualization is therefore **not** implemented: per the issue,
  it's only warranted "if profiling says it's needed," and at 120 columns the
  windowed grid is well inside budget. If a future table pushes column counts
  far higher, revisit with the same measurement.
- **Rows per page:** the whole page (100) is safe; the DOM footprint is bounded
  by the viewport (~24–44 rows) regardless of page height, so raising
  `PAGE_SIZE` later stays render-safe (re-check the FRE-34 memory budget first).

### Reproducing the measurement

The webview render numbers are gathered manually (there is no automated webview
timing harness — the FRE-34 budgets cover the db layer). Build a wide fixture
(e.g. `common::FixtureDb::wide(5000, 120)` dumped to a file, or any 100+‑column
table), open it in the app, and in a `document::eval` probe count
`#dv-grid tbody tr` / `td` and time a forced relayout
(`grid.style.width` toggle + `void grid.scrollHeight`), comparing the windowed
grid against a build that renders the whole page.
