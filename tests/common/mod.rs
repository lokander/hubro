//! Shared test support: builds SQLite fixture databases in temp directories.
//!
//! Fixtures are generated at test setup time — no binary files are checked
//! in. Each [`FixtureDb`] owns its temp dir, so the database file lives until
//! the fixture is dropped.

// Each integration-test binary compiles its own copy of this module and uses
// only a subset of it, so unused-item lints would fire spuriously.
#![allow(dead_code)]

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use hubro::db::DbPool;
use sqlx::sqlite::SqlitePoolOptions;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqliteSynchronous};
use sqlx::SqlitePool;

/// Schema and seed data for [`FixtureDb::full`]. Covers, in one database:
///
/// - tables and a view
/// - unique, non-unique, and expression indexes
/// - a composite primary key (`albums`) and a `WITHOUT ROWID` table
///   (`settings`)
/// - a single-column FK, a multi-column FK, and an FK referencing the
///   target's implicit primary key (`tracks.composer_id REFERENCES artists`)
/// - all five storage classes (NULL, INTEGER, REAL, TEXT, BLOB) in the
///   `artists` rows, including an empty blob
/// - weird identifiers: an SQL keyword as table name (`"order"`) with a
///   keyword column (`"group"`), and a table with an embedded double quote
///   and a space in its name (`"we""ird table"`) whose columns have a space,
///   unicode, and a keyword as names
const FULL_SCHEMA: &str = r#"
    CREATE TABLE artists (
        id INTEGER PRIMARY KEY,
        name TEXT NOT NULL,
        rating REAL,
        cover BLOB,
        notes TEXT DEFAULT 'none'
    );
    CREATE TABLE albums (
        artist_id INTEGER NOT NULL REFERENCES artists(id),
        seq INTEGER NOT NULL,
        title TEXT NOT NULL,
        PRIMARY KEY (artist_id, seq)
    );
    CREATE TABLE tracks (
        id INTEGER PRIMARY KEY,
        title TEXT NOT NULL,
        album_artist_id INTEGER,
        album_seq INTEGER,
        composer_id INTEGER REFERENCES artists,
        FOREIGN KEY (album_artist_id, album_seq)
            REFERENCES albums (artist_id, seq)
    );
    CREATE TABLE settings (
        key TEXT NOT NULL,
        scope TEXT NOT NULL,
        value TEXT,
        PRIMARY KEY (key, scope)
    ) WITHOUT ROWID;
    CREATE TABLE "order" (
        id INTEGER PRIMARY KEY,
        "group" TEXT
    );
    CREATE TABLE "we""ird table" (
        "col name" TEXT,
        "übercol" REAL,
        "select" INTEGER PRIMARY KEY
    );
    CREATE UNIQUE INDEX idx_artists_name ON artists(name);
    CREATE INDEX idx_albums_title ON albums(title);
    CREATE INDEX idx_tracks_title_lower ON tracks(lower(title));
    CREATE VIEW artist_overview AS
        SELECT a.id, a.name, count(al.seq) AS album_count
        FROM artists a LEFT JOIN albums al ON al.artist_id = a.id
        GROUP BY a.id, a.name;
    INSERT INTO artists (id, name, rating, cover, notes) VALUES
        (1, 'Ana', 4.5, x'010203', NULL),
        (2, 'Bo', NULL, NULL, 'good'),
        (3, 'Cleo', 3.0, x'', 'ok');
    INSERT INTO albums (artist_id, seq, title) VALUES
        (1, 1, 'First'),
        (1, 2, 'Second'),
        (2, 1, 'Solo');
    INSERT INTO tracks (id, title, album_artist_id, album_seq, composer_id) VALUES
        (1, 'Opening', 1, 1, 2),
        (2, 'Closing', 1, 2, NULL);
    INSERT INTO settings (key, scope, value) VALUES
        ('theme', 'user', 'dark'),
        ('theme', 'default', 'light');
    INSERT INTO "order" (id, "group") VALUES (1, 'g1'), (2, NULL);
    INSERT INTO "we""ird table" ("col name", "übercol", "select") VALUES
        ('first row', 1.5, 1),
        ('second row', NULL, 2),
        ('другой row', 2.5, 3);
"#;

/// A SQLite database file generated in its own temp directory.
pub struct FixtureDb {
    // Held so the temp dir (and the db file in it) outlives the fixture.
    // `None` for cached perf fixtures, which live under `target/` and persist
    // across runs on purpose (see [`FixtureDb::cached`]).
    _dir: Option<tempfile::TempDir>,
    path: PathBuf,
}

impl FixtureDb {
    /// Creates a database and runs the given setup SQL against it
    /// (multiple `;`-separated statements are allowed).
    pub async fn with_sql(sql: &str) -> FixtureDb {
        let dir = tempfile::tempdir().expect("create temp dir for fixture db");
        let path = dir.path().join("fixture.db");
        let options = SqliteConnectOptions::new()
            .filename(&path)
            .create_if_missing(true);
        let setup = SqlitePool::connect_with(options)
            .await
            .expect("create fixture db");
        if !sql.trim().is_empty() {
            sqlx::query(sql)
                .execute(&setup)
                .await
                .expect("run fixture setup SQL");
        }
        setup.close().await;
        FixtureDb {
            _dir: Some(dir),
            path,
        }
    }

    /// The full fixture (see [`FULL_SCHEMA`] for everything it covers).
    pub async fn full() -> FixtureDb {
        Self::with_sql(FULL_SCHEMA).await
    }

    /// A `numbers(n INTEGER PRIMARY KEY, label TEXT, score REAL)` table with
    /// rows `1..=count`. Labels are zero-padded (`row 01`) so text sort order
    /// matches numeric order; `score` is NULL on every fifth row and `n * 0.5`
    /// otherwise, giving sort tests a mix of NULLs and reals.
    pub async fn numbers(count: u32) -> FixtureDb {
        let mut sql =
            String::from("CREATE TABLE numbers (n INTEGER PRIMARY KEY, label TEXT, score REAL);\n");
        for n in 1..=count {
            let score = if n % 5 == 0 {
                "NULL".to_string()
            } else {
                format!("{:.1}", f64::from(n) * 0.5)
            };
            sql.push_str(&format!(
                "INSERT INTO numbers (n, label, score) VALUES ({n}, 'row {n:02}', {score});\n"
            ));
        }
        Self::with_sql(&sql).await
    }

    /// Path of the database file (inside the fixture's temp dir).
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Opens the fixture through the app's database layer.
    pub async fn open(&self) -> DbPool {
        DbPool::open_sqlite(&self.path)
            .await
            .expect("open fixture db")
    }
}

// ---------------------------------------------------------------------------
// Performance fixtures and budgets (FRE-34)
//
// The generators below build the large databases the perf harness in
// `tests/perf.rs` measures against. They are parameterizable so ordinary tests
// can exercise them at tiny scale (fast, in a temp dir) while the ignored perf
// tests use the full [`perf_scale`] constants and cache the result under
// `target/perf-fixtures/` so repeated bench runs don't rebuild.
//
// Generation runs with `synchronous=OFF` and `journal_mode=MEMORY` and inserts
// in large multi-row batches inside a single transaction, so building a
// million rows takes tens of seconds rather than minutes. Those pragmas only
// affect the throwaway generation connection; the app opens the file normally.
// ---------------------------------------------------------------------------

/// Full-scale parameters for the perf harness. Tests pass smaller numbers.
pub mod perf_scale {
    /// Rows in the wide table (`time-to-first-rows`, paging, `count_rows`).
    pub const WIDE_ROWS: u64 = 1_000_000;
    /// Columns in the wide table (mixed INTEGER/TEXT/REAL). 100+ per the issue.
    pub const WIDE_COLS: usize = 100;
    /// Tables in the schema-heavy database (`schema-load`).
    pub const SCHEMA_TABLES: usize = 300;
    /// Payload size of each multi-MB value row.
    pub const BIG_VALUE_BYTES: usize = 4 * 1024 * 1024;
    /// Number of multi-MB value rows.
    pub const BIG_VALUE_ROWS: u64 = 4;
}

/// Performance budgets — the contract FRE-32 (virtual scrolling) and FRE-33
/// (bounded memory / streaming) must keep meeting. Thresholds are p50 targets
/// measured on the dev machine and set generously (roughly 3x the observed
/// baseline) so they catch real regressions without being flaky on slower CI
/// hardware. See `docs/PERFORMANCE.md` for the recorded baseline and rationale.
pub mod budgets {
    use std::time::Duration;

    /// Opening a pool against the 1M-row database (startup proxy: the real
    /// Dioxus window startup is out of scope for this db-layer harness).
    pub const CONNECTION_OPEN: Duration = Duration::from_millis(150);
    /// `count_rows` + `fetch_page(LIMIT 100, OFFSET 0)` on the 1M-row table —
    /// what the grid does when a table is first opened. Dominated by the
    /// COUNT(*) scan below.
    pub const TIME_TO_FIRST_ROWS: Duration = Duration::from_millis(900);
    /// `count_rows` alone (COUNT(*) full scan) on the 1M-row table. ~3x the
    /// dev-machine baseline so CI hardware (~2-3x slower) still passes while an
    /// order-of-magnitude regression (e.g. decoding every row) fails.
    pub const COUNT_ROWS: Duration = Duration::from_millis(800);
    /// `fetch_page(LIMIT 100, OFFSET 0)` — a shallow page.
    pub const PAGE_NAV_SHALLOW: Duration = Duration::from_millis(100);
    /// `fetch_page(LIMIT 100)` near the end of the 1M-row table. OFFSET paging
    /// is O(offset) in SQLite, so this is deliberately the loosest budget; it
    /// is exactly what keyset pagination (FRE-32) should later collapse.
    pub const PAGE_NAV_DEEP: Duration = Duration::from_millis(3000);
    /// `introspect()` on the 300-table database.
    pub const SCHEMA_LOAD: Duration = Duration::from_millis(1500);
    /// `fetch_page(LIMIT 100)` on the multi-MB-value table (a few fat rows).
    pub const BIG_VALUE_PAGE: Duration = Duration::from_millis(500);
}

/// Opens a generation-only pool: a single connection with durability traded
/// away for speed. Never use these pragmas for the app's real connections.
async fn gen_pool(path: &Path) -> SqlitePool {
    let options = SqliteConnectOptions::new()
        .filename(path)
        .create_if_missing(true)
        .synchronous(SqliteSynchronous::Off)
        .journal_mode(SqliteJournalMode::Memory);
    SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(options)
        .await
        .expect("open generation pool")
}

/// Directory where cached perf fixtures live (removed by `cargo clean`).
fn perf_cache_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/perf-fixtures")
}

impl FixtureDb {
    /// Returns a cached fixture at `target/perf-fixtures/<name>.db`, building it
    /// with `build` on the first call (or when `HUBRO_PERF_REBUILD` is set).
    /// The database is built to a temp path and atomically renamed, so an
    /// interrupted build never leaves a half-written cache behind.
    async fn cached<F, Fut>(name: &str, build: F) -> FixtureDb
    where
        F: FnOnce(PathBuf) -> Fut,
        Fut: std::future::Future<Output = ()>,
    {
        let dir = perf_cache_dir();
        std::fs::create_dir_all(&dir).expect("create perf cache dir");
        let path = dir.join(format!("{name}.db"));
        let rebuild = std::env::var_os("HUBRO_PERF_REBUILD").is_some();
        if rebuild || !path.exists() {
            let tmp = dir.join(format!("{name}.db.building"));
            let _ = std::fs::remove_file(&tmp);
            build(tmp.clone()).await;
            std::fs::rename(&tmp, &path).expect("publish cached perf fixture");
        }
        FixtureDb { _dir: None, path }
    }

    /// Builds a wide, tall table `wide(id INTEGER PRIMARY KEY, c001..cNNN)` with
    /// `rows` rows and `cols` total columns of mixed INTEGER/TEXT/REAL types.
    /// Non-cached (temp dir): use for small-scale tests.
    pub async fn wide(rows: u64, cols: usize) -> FixtureDb {
        let dir = tempfile::tempdir().expect("create temp dir for wide fixture");
        let path = dir.path().join("wide.db");
        generate_wide(&path, rows, cols).await;
        FixtureDb {
            _dir: Some(dir),
            path,
        }
    }

    /// Cached full-scale wide table (`perf_scale::WIDE_ROWS` x `WIDE_COLS`).
    pub async fn wide_cached(rows: u64, cols: usize) -> FixtureDb {
        Self::cached(&format!("wide_{rows}x{cols}"), move |path| async move {
            generate_wide(&path, rows, cols).await;
        })
        .await
    }

    /// Builds `big_values(id, kind, payload TEXT, blob_payload BLOB)` with
    /// `rows` rows, each holding a `bytes`-sized TEXT and BLOB payload.
    pub async fn big_values(rows: u64, bytes: usize) -> FixtureDb {
        let dir = tempfile::tempdir().expect("create temp dir for big-values fixture");
        let path = dir.path().join("big_values.db");
        generate_big_values(&path, rows, bytes).await;
        FixtureDb {
            _dir: Some(dir),
            path,
        }
    }

    /// Cached multi-MB-value fixture.
    pub async fn big_values_cached(rows: u64, bytes: usize) -> FixtureDb {
        Self::cached(
            &format!("big_values_{rows}x{bytes}"),
            move |path| async move {
                generate_big_values(&path, rows, bytes).await;
            },
        )
        .await
    }

    /// Builds a schema-heavy database of `tables` tables, each with a few
    /// columns, an index, and a couple of rows so introspection does real work.
    pub async fn many_tables(tables: usize) -> FixtureDb {
        let dir = tempfile::tempdir().expect("create temp dir for schema fixture");
        let path = dir.path().join("schema.db");
        generate_many_tables(&path, tables).await;
        FixtureDb {
            _dir: Some(dir),
            path,
        }
    }

    /// Cached schema-heavy database.
    pub async fn many_tables_cached(tables: usize) -> FixtureDb {
        Self::cached(&format!("schema_{tables}"), move |path| async move {
            generate_many_tables(&path, tables).await;
        })
        .await
    }
}

/// The declared type of wide column `c` (column 0 is the INTEGER PK `id`).
fn wide_col_type(c: usize) -> &'static str {
    match c % 4 {
        0 => "INTEGER",
        2 => "REAL",
        _ => "TEXT",
    }
}

async fn generate_wide(path: &Path, rows: u64, cols: usize) {
    assert!(cols >= 2, "wide table needs at least id + one column");
    let pool = gen_pool(path).await;

    let mut create = String::from("CREATE TABLE wide (\n  id INTEGER PRIMARY KEY");
    let mut col_list = String::from("id");
    for c in 1..cols {
        write!(create, ",\n  c{c:03} {}", wide_col_type(c)).unwrap();
        write!(col_list, ", c{c:03}").unwrap();
    }
    create.push_str("\n)");
    sqlx::query(&create)
        .execute(&pool)
        .await
        .expect("create wide table");

    const BATCH: u64 = 1000;
    let insert_head = format!("INSERT INTO wide ({col_list}) VALUES ");
    let mut tx = pool.begin().await.expect("begin wide insert");
    let mut next = 1u64;
    while next <= rows {
        let end = (next + BATCH - 1).min(rows);
        let mut sql =
            String::with_capacity(insert_head.len() + (end - next + 1) as usize * cols * 10);
        sql.push_str(&insert_head);
        for r in next..=end {
            if r != next {
                sql.push(',');
            }
            sql.push('(');
            write!(sql, "{r}").unwrap();
            for c in 1..cols {
                sql.push(',');
                match c % 4 {
                    0 => write!(
                        sql,
                        "{}",
                        r.wrapping_mul(31).wrapping_add(c as u64) % 100_000
                    )
                    .unwrap(),
                    2 => write!(sql, "{:.3}", (r as f64 + c as f64) * 0.25).unwrap(),
                    _ => write!(sql, "'r{r}c{c}'").unwrap(),
                }
            }
            sql.push(')');
        }
        sqlx::query(&sql)
            .execute(&mut *tx)
            .await
            .expect("insert wide batch");
        next = end + 1;
    }
    tx.commit().await.expect("commit wide insert");
    pool.close().await;
}

async fn generate_big_values(path: &Path, rows: u64, bytes: usize) {
    let pool = gen_pool(path).await;
    sqlx::query(
        "CREATE TABLE big_values (
            id INTEGER PRIMARY KEY,
            kind TEXT NOT NULL,
            payload TEXT NOT NULL,
            blob_payload BLOB NOT NULL
        )",
    )
    .execute(&pool)
    .await
    .expect("create big_values table");

    for id in 1..=rows {
        // A repeated marker keeps generation cheap while filling `bytes` bytes.
        let unit = format!("row-{id:04}-");
        let text: String = unit.repeat(bytes / unit.len() + 1)[..bytes].to_string();
        let blob = text.clone().into_bytes();
        sqlx::query("INSERT INTO big_values (id, kind, payload, blob_payload) VALUES (?, ?, ?, ?)")
            .bind(id as i64)
            .bind(format!("payload {id}"))
            .bind(text)
            .bind(blob)
            .execute(&pool)
            .await
            .expect("insert big value");
    }
    pool.close().await;
}

async fn generate_many_tables(path: &Path, tables: usize) {
    let pool = gen_pool(path).await;
    let mut tx = pool.begin().await.expect("begin schema build");
    for i in 0..tables {
        let ddl = format!(
            "CREATE TABLE t{i:04} (
                id INTEGER PRIMARY KEY,
                label TEXT NOT NULL,
                amount REAL,
                flag INTEGER
            );
            CREATE INDEX idx_t{i:04}_label ON t{i:04}(label);
            INSERT INTO t{i:04} (id, label, amount, flag) VALUES
                (1, 'first {i}', 1.5, 1),
                (2, 'second {i}', 2.5, 0);"
        );
        sqlx::query(&ddl)
            .execute(&mut *tx)
            .await
            .expect("create schema table");
    }
    tx.commit().await.expect("commit schema build");
    pool.close().await;
}

// ---------------------------------------------------------------------------
// Timing harness
// ---------------------------------------------------------------------------

/// Collected timings for one operation, with percentile helpers.
pub struct Timings {
    label: String,
    samples: Vec<Duration>,
}

impl Timings {
    /// Runs `op` `iters` times (plus one warmup that is discarded) and records
    /// each elapsed time. `op` is an async closure returning a future.
    pub async fn measure<F, Fut>(label: &str, iters: usize, mut op: F) -> Timings
    where
        F: FnMut() -> Fut,
        Fut: std::future::Future<Output = ()>,
    {
        // Warmup: prime OS/page caches so we measure steady-state, not cold I/O.
        op().await;
        let mut samples = Vec::with_capacity(iters);
        for _ in 0..iters {
            let start = Instant::now();
            op().await;
            samples.push(start.elapsed());
        }
        samples.sort_unstable();
        Timings {
            label: label.to_string(),
            samples,
        }
    }

    fn percentile(&self, pct: f64) -> Duration {
        assert!(!self.samples.is_empty(), "no samples for {}", self.label);
        let rank = (pct / 100.0 * (self.samples.len() as f64 - 1.0)).round() as usize;
        self.samples[rank.min(self.samples.len() - 1)]
    }

    pub fn p50(&self) -> Duration {
        self.percentile(50.0)
    }

    pub fn p95(&self) -> Duration {
        self.percentile(95.0)
    }

    /// Prints a one-line report and asserts p50 is within `budget`.
    pub fn report_and_assert(&self, budget: Duration) {
        let pass = self.p50() <= budget;
        println!(
            "{:<22} n={:<3} p50={:>9.2?} p95={:>9.2?} budget={:>7.2?} {}",
            self.label,
            self.samples.len(),
            self.p50(),
            self.p95(),
            budget,
            if pass { "PASS" } else { "FAIL" },
        );
        assert!(
            pass,
            "{}: p50 {:?} exceeds budget {:?}",
            self.label,
            self.p50(),
            budget,
        );
    }
}

// ---------------------------------------------------------------------------
// A database per test binary (FRE-127)
// ---------------------------------------------------------------------------

/// The SQL Server URL this test binary should use: `HUBRO_MSSQL_TEST_URL` with
/// the database swapped for one created for this process alone. `None` when
/// the variable is unset, which is how every SQL Server suite skips.
///
/// See [`per_process_database`] for why.
pub async fn mssql_test_url() -> Option<String> {
    static URL: tokio::sync::OnceCell<Option<String>> = tokio::sync::OnceCell::const_new();
    URL.get_or_init(|| per_process_database(Engine::SqlServer))
        .await
        .clone()
}

/// The Postgres URL this test binary should use — the same treatment, for the
/// same reason. `HUBRO_PG_TEST_URL`, database swapped.
pub async fn pg_test_url() -> Option<String> {
    static URL: tokio::sync::OnceCell<Option<String>> = tokio::sync::OnceCell::const_new();
    URL.get_or_init(|| per_process_database(Engine::Postgres))
        .await
        .clone()
}

/// Which server a per-process database is being built on. The two differ in
/// how they are opened, how identifiers are quoted, and — the awkward one —
/// how the sweep below can tell a database's age.
#[derive(Clone, Copy)]
enum Engine {
    Postgres,
    SqlServer,
}

impl Engine {
    fn env(self) -> &'static str {
        match self {
            Engine::Postgres => "HUBRO_PG_TEST_URL",
            Engine::SqlServer => "HUBRO_MSSQL_TEST_URL",
        }
    }

    async fn open(self, url: &str) -> Result<DbPool, hubro::db::DbError> {
        match self {
            Engine::Postgres => DbPool::open_postgres(url).await,
            Engine::SqlServer => DbPool::open_mssql(url).await,
        }
    }

    fn quote(self, name: &str) -> String {
        match self {
            Engine::Postgres => format!("\"{name}\""),
            Engine::SqlServer => format!("[{name}]"),
        }
    }

    /// Lists leftover databases old enough to sweep.
    ///
    /// SQL Server records `create_date`, so it can be asked directly. Postgres
    /// records no creation time for a database at all, which is why the name
    /// carries a unix timestamp — the alternative, reading it off the data
    /// directory with `pg_stat_file`, needs superuser and a server-local path.
    fn stale_query(self) -> String {
        match self {
            // One whole-name regex, so the `::bigint` below can only ever see
            // digits. Splitting the shape check from the cast across two
            // ANDed quals left the cast's safety resting on the planner not
            // reordering them — true here, but not something a reader should
            // have to verify. It also matches exactly: `_` is a wildcard in
            // LIKE, so `'{TEST_DB_PREFIX}%'` would also match `hubroXtestY…`.
            Engine::Postgres => format!(
                "SELECT datname FROM pg_database
                  WHERE datname ~ '^{TEST_DB_PREFIX}[0-9]+_[0-9]+$'
                    AND split_part(datname, '_', 4)::bigint
                        < EXTRACT(EPOCH FROM now()) - {STALE_TEST_DB_SECONDS}"
            ),
            // T-SQL has no regex; `[_]` escapes the wildcard, and `create_date`
            // means the name's timestamp is not load-bearing here.
            Engine::SqlServer => format!(
                "SELECT name FROM sys.databases
                  WHERE name LIKE 'hubro[_]test[_]%'
                    AND create_date < DATEADD(second, -{STALE_TEST_DB_SECONDS}, GETDATE())"
            ),
        }
    }
}

/// How old a leftover database must be before a later run sweeps it.
///
/// The only real race is a binary that has created its database but not yet
/// connected to it — a window of well under a millisecond. A minute is
/// several orders beyond that, and 15-60x the longest test binary observed
/// (0.6-4s), while keeping leftovers to roughly a minute's worth rather than
/// ten. A suite that *does* run longer is safe regardless: `DROP DATABASE`
/// fails on a database with live connections — including merely idle ones,
/// which was checked on both engines rather than assumed — and the sweep
/// tolerates that failure.
const STALE_TEST_DB_SECONDS: u64 = 60;

/// Prefix for the per-process databases, and so also the sweep's pattern.
const TEST_DB_PREFIX: &str = "hubro_test_";

/// Builds this process a database of its own and returns the URL naming it.
///
/// # Why
///
/// The gated server suites built their fixtures in whatever database the URL
/// named — in practice one shared database per engine — using the *same
/// fixture names*. Any two suites running at once therefore dropped each
/// other's tables.
///
/// Note what does **not** cause that: cargo runs test *targets* sequentially
/// (measured — never more than one test binary alive at a time), so a plain
/// `cargo test` was never the trigger. Two concurrent `cargo test`
/// **invocations** are: two agents on one container set, which the root
/// CLAUDE.md warns about and which this does not make safe — the databases in
/// the fixture *set* are still shared. Measured on stock containers, two
/// concurrent copies of one suite: `db_sqlserver` 4-9 failures each,
/// `db_postgres` 4-13, with errors like "Invalid object name" and
/// `relation "bit_key" does not exist`.
///
/// SQL Server's deadlock-victim kill (1205) is a second, smaller effect and
/// was never only about concurrency: 30 solo runs of `db_ddl` against the
/// shared `master` produced 4 failures, all 1205, against 0 in 80 runs here.
///
/// A database per process fixes both halves and, unlike a per-test name
/// prefix, needs no change to a single fixture: names stay as they were and
/// only the URL moves. It also isolates the *catalog*, which is where the
/// deadlocks were coming from.
///
/// # Lifetime
///
/// Created on first use and left behind at exit: a Rust test binary has no
/// teardown hook, and there is no safe point to run an async drop from. Each
/// run instead sweeps databases older than [`STALE_TEST_DB_SECONDS`] first,
/// which self-cleans without a race.
async fn per_process_database(engine: Engine) -> Option<String> {
    let base = match std::env::var(engine.env()) {
        Ok(url) => url,
        Err(_) => {
            eprintln!("skipping: {} not set", engine.env());
            return None;
        }
    };
    let admin = engine
        .open(&base)
        .await
        .unwrap_or_else(|e| panic!("{} must be connectable: {e}", engine.env()));

    // Best effort throughout: a sweep that fails leaves rubbish behind, which
    // is not a reason to fail the suite. Dropped one at a time and ignored
    // individually, because a database still in use makes `DROP DATABASE` fail
    // — and in one batch that failure would abort the rest of the sweep, so
    // the tidying would stop working exactly when there is something to tidy.
    match admin.query(&engine.stale_query()).await {
        Ok(stale) => {
            for row in &stale.rows {
                if let Some(hubro::db::Value::Text(name)) = row.first() {
                    let _ = admin
                        .query(&format!("DROP DATABASE {}", engine.quote(name)))
                        .await;
                }
            }
        }
        // Said out loud rather than swallowed. A sweep that silently stopped
        // working would look exactly like a sweep with nothing to do, and the
        // only symptom would be leftovers accumulating for months.
        Err(e) => eprintln!("warning: could not list stale test databases: {e}"),
    }

    // PID keeps concurrent binaries apart; the timestamp keeps successive runs
    // of the same binary apart, since PIDs are reused — and it is what the
    // Postgres sweep reads the age from, so its format is load-bearing.
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let name = format!("{TEST_DB_PREFIX}{}_{secs}", std::process::id());
    admin
        .query(&format!("CREATE DATABASE {}", engine.quote(&name)))
        .await
        .unwrap_or_else(|e| panic!("could not create the test database {name}: {e}"));
    admin.close().await;
    Some(with_database(&base, &name))
}

/// Rewrites a database URL to name a different database.
///
/// The database is the URL's path, between the authority's `/` and the query.
/// Written out here rather than pulled from `src/db` because nothing there
/// needs it: the app connects to the database the user named.
pub fn with_database(url: &str, database: &str) -> String {
    let (scheme, rest) = url.split_once("://").unwrap_or(("", url));
    // The authority ends at the first `/` — or, when there is no path at all,
    // at the `?` that starts the query. Missing that second case appended the
    // database *inside the last query value* (`…&trustServerCertificate=true/NEW`),
    // which then fails to parse.
    let authority_end = rest
        .find('/')
        .map(|at| (at, at + 1))
        .or_else(|| rest.find('?').map(|at| (at, at)))
        .unwrap_or((rest.len(), rest.len()));
    let (authority, tail) = (&rest[..authority_end.0], &rest[authority_end.1..]);
    let query = tail.find('?').map(|at| &tail[at..]).unwrap_or("");
    format!("{scheme}://{authority}/{database}{query}")
}

#[test]
fn a_rewritten_url_keeps_everything_but_the_database() {
    // The credentials, port and parameters all have to survive, or the suites
    // silently reconnect to the wrong place — or not at all.
    assert_eq!(
        with_database(
            "mssql://sa:Str0ng%21Passw0rd@localhost:14333/master?encrypt=on&trustServerCertificate=true",
            "hubro_test_1_2"
        ),
        "mssql://sa:Str0ng%21Passw0rd@localhost:14333/hubro_test_1_2?encrypt=on&trustServerCertificate=true"
    );
    // Percent-encoded userinfo passes through untouched. The database is
    // located as the first `/` after the scheme, which is correct precisely
    // because userinfo may not contain a bare `/` — a password that does has
    // to be encoded, and would not survive sqlx or tiberius either. This case
    // is here because the naive reading of "the first slash" looks wrong until
    // you check that.
    assert_eq!(
        with_database("postgres://u:a%2Fb@h:5432/demo", "db"),
        "postgres://u:a%2Fb@h:5432/db"
    );
    assert_eq!(
        with_database(
            "postgres://tester:testpass@localhost:5433/demo",
            "hubro_test_1_2"
        ),
        "postgres://tester:testpass@localhost:5433/hubro_test_1_2"
    );
    // No path at all, and — the case the first version of this test claimed
    // to cover and didn't — no path *with* a query, where the database was
    // being appended inside the last parameter value.
    assert_eq!(
        with_database("postgres://u@h:5432", "db"),
        "postgres://u@h:5432/db"
    );
    assert_eq!(
        with_database(
            "mssql://sa:p@h:14333?encrypt=on&trustServerCertificate=true",
            "db"
        ),
        "mssql://sa:p@h:14333/db?encrypt=on&trustServerCertificate=true"
    );
    // An IPv6 literal host keeps its brackets and colons.
    assert_eq!(
        with_database("postgres://u@[::1]:5432/demo", "db"),
        "postgres://u@[::1]:5432/db"
    );
    assert_eq!(
        with_database("postgres://u@h:5432/?sslmode=disable", "db"),
        "postgres://u@h:5432/db?sslmode=disable"
    );
}
