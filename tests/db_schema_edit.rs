//! Integration tests for schema editing, against every engine hubro
//! supports — nine of them: SQLite plus the eight behind a `HUBRO_*_TEST_URL`.
//!
//! The unit tests in `src/db/schema_edit.rs` pin what the generator *emits*.
//! These pin what the servers *accept and do* — which is the only thing that
//! settles whether "the operations dialects agree on" was a true claim. Each
//! engine runs the same sequence through the same code the dialog runs
//! ([`schema_op_sql`] then [`run_script`]), and every statement is checked
//! twice: the server accepted it, and re-introspection shows the change it was
//! supposed to make. A `CREATE INDEX` the server accepts and silently ignores
//! (Materialize's arrangements are not indexes in the Postgres sense) would
//! pass the first check and fail the second.
//!
//! The SQLite half always runs. Every other engine skips unless its
//! `HUBRO_*_TEST_URL` is set — see `tests/CLAUDE.md`, and the `docker run`
//! commands in each engine's own suite header.
//!
//! ## What the sweep found (2026-08-11)
//!
//! **Seven of the nine engines apply all seven operations as generated**:
//! SQLite, PostgreSQL, TimescaleDB, Citus, CockroachDB, YugabyteDB and SQL
//! Server. The narrow scope holds: nothing needed a per-engine special case
//! beyond the three dialect differences the generator already makes
//! (`sp_rename`, T-SQL's `ADD` without `COLUMN`, SQLite's `DELETE` for
//! truncate).
//!
//! The two stream-processing engines have partial DDL, and their gaps are
//! recorded as expectations rather than skipped:
//!
//! - **Materialize** refuses `ADD COLUMN` (behind a feature flag that is off by
//!   default), `RENAME COLUMN` (not implemented) and `TRUNCATE` (absent from
//!   its grammar). Create/drop index, rename table and drop table all apply,
//!   and its indexes are visible to introspection.
//! - **RisingWave** refuses `RENAME COLUMN` and `TRUNCATE`, both reported as
//!   "not yet implemented". The other five apply.
//!
//! **Those gaps are deliberately not modelled as capabilities.** `ddl` is one
//! flag, and both engines genuinely have DDL; carving it into per-operation
//! flags is the kind of widening that narrowing schema editing to
//! low-divergence operations exists to avoid, and it would have to be
//! re-derived for every engine added afterwards. What matters is the failure
//! *mode*, which [`assert_supported_except`] pins: every refusal is a message
//! the dialog shows, and no operation is ever accepted while changing nothing.
//! An engine that silently ignored a statement would be the one outcome this
//! design cannot report honestly — so it fails the test rather than being
//! listed as unsupported.
//!
//! Two engines needed their *fixture* written differently — see [`Flavor`].
//! That is their `CREATE TABLE`, not hubro's, and hubro generates no
//! `CREATE TABLE` at all.
//!
//! On **CockroachDB**, note `cockroach_script_dml_rolls_back_but_its_ddl_does_not`
//! in `db_cockroach.rs`: DDL there commits ahead of the transaction. Nothing
//! here depends on a rollback — every operation is a single statement, which is
//! pinned by `every_generated_statement_is_exactly_one_statement`.

mod common;

use std::collections::HashMap;
use std::sync::{Arc, OnceLock};

use common::FixtureDb;
use hubro::db::{
    run_script, schema_edit_refusal, schema_op_sql, split_statements, Capabilities, DbPool,
    Dialect, SchemaOp, TableMeta, Value, EXCLUSION_CONSTRAINT_INDEX, PRIMARY_KEY_INDEX,
    UNIQUE_CONSTRAINT_INDEX,
};

/// What happened to one operation on one engine.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Outcome {
    /// The server ran it and introspection confirms it did what it says.
    Applied,
    /// The server refused the statement. The message is kept so a new engine's
    /// gap is readable from the failure rather than only from the name of the
    /// assertion.
    Rejected(String),
    /// The server accepted it and nothing changed — the failure mode that
    /// looks like success.
    NoEffect,
}

impl Outcome {
    fn applied(&self) -> bool {
        *self == Outcome::Applied
    }
}

/// One engine's run: every operation, in the order they were applied.
type Report = Vec<(&'static str, Outcome)>;

fn find<'a>(tables: &'a [TableMeta], name: &str) -> Option<&'a TableMeta> {
    tables.iter().find(|t| t.name == name)
}

fn table<'a>(tables: &'a [TableMeta], name: &str) -> &'a TableMeta {
    find(tables, name).unwrap_or_else(|| panic!("table {name:?} missing from introspection"))
}

/// The schema-qualified name, as the engine's own introspection reports it —
/// so the row counts below name the same object the generated statement does.
fn qualified(meta: &TableMeta) -> String {
    match &meta.schema {
        Some(schema) => format!("\"{schema}\".\"{}\"", meta.name),
        None => format!("\"{}\"", meta.name),
    }
}

async fn row_count(pool: &DbPool, meta: &TableMeta) -> i64 {
    let sql = format!("SELECT COUNT(*) FROM {}", qualified(meta));
    let result = pool
        .query(&sql)
        .await
        .unwrap_or_else(|e| panic!("counting rows failed: {e}\n{sql}"));
    match result.rows.first().and_then(|row| row.first()) {
        Some(Value::Integer(n)) => *n,
        // Postgres' count() is bigint and decodes as an integer; SQL Server's
        // is int. Anything else is worth failing loudly on.
        other => panic!("unexpected count cell: {other:?}"),
    }
}

/// Runs one operation exactly as the dialog does: generate, split, run through
/// the script path with the connection's own capabilities.
async fn apply(pool: &DbPool, meta: &TableMeta, op: &SchemaOp) -> Result<(), String> {
    let caps = pool.backend_capabilities();
    let sql = schema_op_sql(pool.dialect(), meta, op);
    // The gate the button consults must agree that this is offered; if it
    // refuses, the test is exercising something the UI would never run.
    assert_eq!(
        schema_edit_refusal(caps, pool.dialect(), meta, op),
        None,
        "the gate refused an operation this test is about to run: {sql}"
    );
    let statements = split_statements(&sql, pool.dialect());
    run_script(pool, caps, &statements, |_| {})
        .await
        .map(|_| ())
        .map_err(|err| format!("{}\n---\n{sql}", err.error))
}

/// Applies `op` and reports whether the server took it and whether `check`
/// then sees the change.
async fn step(
    pool: &DbPool,
    meta: &TableMeta,
    op: &SchemaOp,
    label: &'static str,
    report: &mut Report,
    check: impl AsyncFnOnce(&DbPool) -> bool,
) -> bool {
    match apply(pool, meta, op).await {
        Err(err) => {
            report.push((label, Outcome::Rejected(err)));
            false
        }
        Ok(()) => {
            let applied = check(pool).await;
            report.push((
                label,
                if applied {
                    Outcome::Applied
                } else {
                    Outcome::NoEffect
                },
            ));
            applied
        }
    }
}

/// The whole sequence, against one engine's live connection.
///
/// Ordered so each operation runs against the state the previous one left:
/// the index is created before it is dropped, the column added before it is
/// renamed, and the table dropped last. A rejected step does not stop the
/// rest — the point of the sweep is a complete picture of one engine, not the
/// first thing it refuses.
async fn exercise(pool: &DbPool, base: &str, flavor: Flavor) -> Report {
    let mut report = Report::new();
    let index = format!("{base}_by_name");
    let renamed = format!("{base}_renamed");

    // ---- create index -----------------------------------------------------
    let meta = table(&pool.introspect().await.unwrap(), base).clone();
    let op = SchemaOp::CreateIndex {
        name: index.clone(),
        columns: vec!["name".into()],
        unique: false,
    };
    let index_name = index.clone();
    step(
        pool,
        &meta,
        &op,
        "create index",
        &mut report,
        async |pool: &DbPool| {
            let tables = pool.introspect().await.unwrap();
            table(&tables, base)
                .indexes
                .iter()
                .any(|i| i.name.eq_ignore_ascii_case(&index_name) && i.columns == ["name"])
        },
    )
    .await;

    // ---- drop index -------------------------------------------------------
    let op = SchemaOp::DropIndex {
        name: index.clone(),
    };
    let index_name = index.clone();
    step(
        pool,
        &meta,
        &op,
        "drop index",
        &mut report,
        async |pool: &DbPool| {
            let tables = pool.introspect().await.unwrap();
            !table(&tables, base)
                .indexes
                .iter()
                .any(|i| i.name.eq_ignore_ascii_case(&index_name))
        },
    )
    .await;

    // ---- add column -------------------------------------------------------
    let op = SchemaOp::AddColumn {
        name: "note".into(),
        type_name: flavor.text_type.into(),
    };
    step(
        pool,
        &meta,
        &op,
        "add column",
        &mut report,
        async |pool: &DbPool| {
            let tables = pool.introspect().await.unwrap();
            table(&tables, base)
                .columns
                .iter()
                // Nullable is not incidental: it is the whole reason this is
                // the one ADD COLUMN form in scope, and the reason it cannot
                // fail on a table that already has rows.
                .any(|c| c.name == "note" && c.nullable)
        },
    )
    .await;

    // ---- rename column ----------------------------------------------------
    // The *fixture's* column, not the one just added: an engine that refuses
    // `ADD COLUMN` would otherwise skip this step, and a sequence that runs a
    // different number of operations per engine is one where a missing result
    // reads as a pass. Its index was dropped above, so nothing depends on the
    // old name.
    let op = SchemaOp::RenameColumn {
        column: "name".into(),
        new_name: "label".into(),
    };
    step(
        pool,
        &meta,
        &op,
        "rename column",
        &mut report,
        async |pool: &DbPool| {
            let tables = pool.introspect().await.unwrap();
            let columns = &table(&tables, base).columns;
            columns.iter().any(|c| c.name == "label") && !columns.iter().any(|c| c.name == "name")
        },
    )
    .await;

    // ---- truncate ---------------------------------------------------------
    assert!(row_count(pool, &meta).await > 0, "fixture has no rows");
    step(
        pool,
        &meta,
        &SchemaOp::Truncate,
        "truncate",
        &mut report,
        async |pool: &DbPool| {
            let tables = pool.introspect().await.unwrap();
            // Still a table, and now an empty one — a "truncate" that dropped
            // it would satisfy a row count of zero just as well.
            let meta = table(&tables, base);
            row_count(pool, meta).await == 0
        },
    )
    .await;

    // ---- rename table -----------------------------------------------------
    let op = SchemaOp::RenameTable {
        new_name: renamed.clone(),
    };
    let new_name = renamed.clone();
    let was_renamed = step(
        pool,
        &meta,
        &op,
        "rename table",
        &mut report,
        async |pool: &DbPool| {
            let tables = pool.introspect().await.unwrap();
            find(&tables, &new_name).is_some() && find(&tables, base).is_none()
        },
    )
    .await;

    // ---- drop table -------------------------------------------------------
    // Against whichever name the table now answers to, so a refused rename
    // does not turn into a second failure here.
    let tables = pool.introspect().await.unwrap();
    let current = if was_renamed { &renamed } else { base };
    let meta = table(&tables, current).clone();
    let gone = current.to_string();
    step(
        pool,
        &meta,
        &SchemaOp::DropTable,
        "drop table",
        &mut report,
        async |pool: &DbPool| {
            let tables = pool.introspect().await.unwrap();
            find(&tables, &gone).is_none()
        },
    )
    .await;

    report
}

/// Asserts that every operation applied — the claim for a fully supporting
/// engine.
fn assert_all_applied(report: &Report, engine: &str) {
    let failures: Vec<String> = report
        .iter()
        .filter(|(_, outcome)| !outcome.applied())
        .map(|(label, outcome)| format!("  {label}: {outcome:?}"))
        .collect();
    assert!(
        failures.is_empty(),
        "{engine} did not apply every operation:\n{}",
        failures.join("\n")
    );
    // A report that ran nothing would pass the check above for the wrong
    // reason — the same trap as an engine suite that skips and reports green.
    assert_eq!(
        report.len(),
        7,
        "{engine} ran {} operations, not the whole sequence: {report:?}",
        report.len()
    );
}

/// Asserts that exactly `unsupported` failed, and that each of those failed by
/// being *refused* rather than by silently doing nothing.
///
/// The distinction is the point. A refusal is a message the user sees; a
/// no-effect is the dialog reporting success over a database that did not
/// change, which is the one outcome this feature must not have.
fn assert_supported_except(report: &Report, engine: &str, unsupported: &[&str]) {
    for (label, outcome) in report {
        let expected_gap = unsupported.contains(label);
        match (expected_gap, outcome) {
            (false, Outcome::Applied) => {}
            (true, Outcome::Rejected(_)) => {}
            (true, Outcome::Applied) => panic!(
                "{engine} now supports {label:?} — the recorded gap is out of date, \
                 move it out of the unsupported list"
            ),
            (_, Outcome::NoEffect) => panic!(
                "{engine} accepted {label:?} and changed nothing — a silent no-op is worse \
                 than a refusal, because the dialog reports it as done"
            ),
            (false, Outcome::Rejected(err)) => {
                panic!("{engine} refused {label:?}, which it is expected to support: {err}")
            }
        }
    }
    assert_eq!(report.len(), 7, "{engine}: incomplete sequence: {report:?}");
}

// ---------------------------------------------------------------------------
// SQLite — always runs
// ---------------------------------------------------------------------------

/// How one engine's *own* DDL has to be written to get the fixture built.
///
/// Not part of what is under test — hubro never writes a `CREATE TABLE`. It is
/// here because two engines cannot express the ordinary fixture: Materialize
/// rejects a primary key outright, and RisingWave's parser has no
/// parameterised `varchar(n)`. Keeping those in one struct means the operations
/// below run against a comparable table everywhere, instead of the sweep
/// stopping at each engine's table syntax before reaching anything hubro
/// generates.
#[derive(Debug, Clone, Copy)]
struct Flavor {
    /// The text type used for the fixture's column and for the column added by
    /// the `AddColumn` operation. That operation's type is the user's own text
    /// in the real dialog, so pinning one spelling here would be testing the
    /// engine's type parser rather than hubro.
    text_type: &'static str,
    /// Whether the fixture declares a primary key.
    primary_key: bool,
}

impl Flavor {
    /// What every engine but the two exceptions takes.
    const ORDINARY: Flavor = Flavor {
        text_type: "varchar(40)",
        primary_key: true,
    };
}

/// The fixture every engine starts from: two columns and two rows, so a
/// truncate has something to remove and an added column has a row to be null
/// on.
fn fixture_sql(name: &str, flavor: Flavor) -> String {
    let key = if flavor.primary_key {
        " PRIMARY KEY"
    } else {
        ""
    };
    let text = flavor.text_type;
    format!(
        "CREATE TABLE {name} (id int{key}, name {text} NOT NULL);
         INSERT INTO {name} (id, name) VALUES (1, 'ana');
         INSERT INTO {name} (id, name) VALUES (2, 'bo');"
    )
}

#[tokio::test]
async fn sqlite_applies_every_operation() {
    let fixture = FixtureDb::with_sql(&fixture_sql("widgets", Flavor::ORDINARY)).await;
    let pool = fixture.open().await;

    let report = exercise(&pool, "widgets", Flavor::ORDINARY).await;
    assert_all_applied(&report, "SQLite");

    pool.close().await;
}

#[tokio::test]
async fn sqlite_truncate_is_a_delete_and_says_so() {
    // The one operation whose *statement* differs on SQLite. The substitution
    // is verified here rather than only in the unit test, because the reason
    // it is allowed at all is that SQLite really does empty the table this
    // way.
    let fixture = FixtureDb::with_sql(&fixture_sql("widgets", Flavor::ORDINARY)).await;
    let pool = fixture.open().await;
    let tables = pool.introspect().await.unwrap();
    let meta = table(&tables, "widgets");

    let sql = schema_op_sql(Dialect::Sqlite, meta, &SchemaOp::Truncate);
    assert!(sql.starts_with("DELETE FROM"), "{sql}");
    assert!(SchemaOp::Truncate.note(Dialect::Sqlite).is_some());
    apply(&pool, meta, &SchemaOp::Truncate).await.unwrap();
    assert_eq!(row_count(&pool, meta).await, 0);
    // The table is still there, which is what makes it a truncate.
    assert!(find(&pool.introspect().await.unwrap(), "widgets").is_some());

    pool.close().await;
}

/// What one engine did with every index of a table that has both kinds: the
/// indexes its constraints own, and indexes made with `CREATE [UNIQUE] INDEX`.
#[derive(Debug)]
struct ConstraintSweep {
    /// The gate's reason for each index it withheld, sorted. Each was also
    /// sent to the engine, which had to refuse it.
    refused: Vec<&'static str>,
    /// Indexes the gate offered that the engine then dropped.
    dropped: usize,
    /// Indexes the gate offered that the engine refused anyway, with its
    /// message — empty wherever the catalog can tell the two kinds apart.
    offered_but_refused: Vec<String>,
}

/// Drives the Drop button's decision over every index of `name`: refused by
/// the gate must mean refused by the engine (with a message containing
/// `engine_refusal`), and offered must mean dropped — except what lands in
/// [`ConstraintSweep::offered_but_refused`], which each caller pins.
async fn constraint_sweep(
    pool: &DbPool,
    name: &str,
    setup: &[String],
    engine_refusal: &str,
) -> ConstraintSweep {
    for statement in setup {
        pool.execute(statement)
            .await
            .unwrap_or_else(|e| panic!("setup failed: {e}\n{statement}"));
    }
    let tables = pool.introspect().await.unwrap();
    let meta = table(&tables, name);
    let caps = pool.backend_capabilities();
    let mut sweep = ConstraintSweep {
        refused: Vec::new(),
        dropped: 0,
        offered_but_refused: Vec::new(),
    };
    for index in &meta.indexes {
        let op = SchemaOp::DropIndex {
            name: index.name.clone(),
        };
        match schema_edit_refusal(caps, pool.dialect(), meta, &op) {
            Some(reason) => {
                sweep.refused.push(reason);
                let sql = schema_op_sql(pool.dialect(), meta, &op);
                let err = pool.execute(&sql).await.expect_err(&sql);
                assert!(err.to_string().contains(engine_refusal), "{err}");
            }
            None => match apply(pool, meta, &op).await {
                Ok(()) => sweep.dropped += 1,
                Err(err) => sweep.offered_but_refused.push(err),
            },
        }
    }
    sweep.refused.sort();
    let left = pool.introspect().await.unwrap();
    assert_eq!(
        table(&left, name).indexes.len(),
        sweep.refused.len() + sweep.offered_but_refused.len(),
        "exactly the indexes the engine refused remain: {sweep:?}"
    );
    sweep
}

/// The fixture every engine with constraints shares: a primary key, a UNIQUE
/// column, a two-column UNIQUE constraint, and two indexes the user made, one
/// of them unique — so `unique` alone cannot be what decides. `extra` adds
/// engine-specific columns or constraints.
///
/// A second table, `{name}_ref`, holds a foreign key to the primary key. On
/// Postgres a foreign key's `pg_constraint.conindid` names the index it leans
/// on — the *referenced* table's primary-key index — so a join that took any
/// constraint naming an index, rather than only `p`/`u`/`x`, would see that
/// index as owned by an `f` and offer to drop it. The sweep refuses the
/// primary key's index only if the filter holds.
fn constraint_fixture(name: &str, text_type: &str, extra: &str) -> Vec<String> {
    let mut setup = drop_constraint_fixture(name);
    setup.extend([
        format!(
            "CREATE TABLE {name} (code {text_type} PRIMARY KEY, slug {text_type} UNIQUE, \
             a INT, b INT, UNIQUE (a, b){extra})"
        ),
        format!("CREATE TABLE {name}_ref (code {text_type} REFERENCES {name} (code))"),
        format!("CREATE UNIQUE INDEX {name}_made_unique ON {name} (a)"),
        format!("CREATE INDEX {name}_made ON {name} (b)"),
    ]);
    setup
}

/// Drops [`constraint_fixture`]'s two tables, the referencing one first.
fn drop_constraint_fixture(name: &str) -> Vec<String> {
    vec![
        format!("DROP TABLE IF EXISTS {name}_ref"),
        format!("DROP TABLE IF EXISTS {name}"),
    ]
}

async fn teardown(pool: &DbPool, name: &str) {
    for statement in drop_constraint_fixture(name) {
        pool.execute(&statement).await.unwrap();
    }
}

fn sorted(mut reasons: Vec<&'static str>) -> Vec<&'static str> {
    reasons.sort();
    reasons
}

#[tokio::test]
async fn sqlite_an_index_built_for_a_constraint_is_not_offered_for_drop() {
    let fixture = FixtureDb::with_sql("").await;
    let pool = fixture.open().await;
    let setup = constraint_fixture("ci_lite", "TEXT", "");
    let sweep = constraint_sweep(
        &pool,
        "ci_lite",
        &setup,
        "associated with UNIQUE or PRIMARY KEY constraint",
    )
    .await;
    assert_eq!(
        sweep.refused,
        sorted(vec![
            PRIMARY_KEY_INDEX,
            UNIQUE_CONSTRAINT_INDEX,
            UNIQUE_CONSTRAINT_INDEX
        ])
    );
    assert_eq!(sweep.dropped, 2, "{sweep:?}");
    assert!(sweep.offered_but_refused.is_empty(), "{sweep:?}");
    pool.close().await;
}

#[tokio::test]
async fn a_read_only_connection_is_offered_nothing_on_any_dialect() {
    // The gate, against a live table's real metadata rather than a
    // hand-built one — so the object's kind and the connection's capabilities
    // are both the ones the app would see.
    let fixture = FixtureDb::with_sql(&fixture_sql("widgets", Flavor::ORDINARY)).await;
    let pool = fixture.open().await;
    let tables = pool.introspect().await.unwrap();
    let meta = table(&tables, "widgets");
    let read_only = Capabilities::FULL.read_only();

    for op in [
        SchemaOp::DropTable,
        SchemaOp::Truncate,
        SchemaOp::AddColumn {
            name: "c".into(),
            type_name: "int".into(),
        },
        SchemaOp::CreateIndex {
            name: "i".into(),
            columns: vec!["name".into()],
            unique: false,
        },
    ] {
        assert!(
            schema_edit_refusal(read_only, pool.dialect(), meta, &op).is_some(),
            "{op:?} was offered on a read-only connection"
        );
    }

    pool.close().await;
}

// ---------------------------------------------------------------------------
// Postgres and the engines that speak its protocol
// ---------------------------------------------------------------------------

/// Creates the fixture on a live server, dropping any leftover of the same
/// name first. Returns the table name, which is suffixed per engine so two
/// suites pointed at one server cannot collide.
async fn seed(pool: &DbPool, name: &str, flavor: Flavor) {
    let _ = pool.query(&format!("DROP TABLE IF EXISTS {name}")).await;
    for statement in split_statements(&fixture_sql(name, flavor), pool.dialect()) {
        pool.query(&statement)
            .await
            .unwrap_or_else(|e| panic!("seeding {name} failed: {e}\n{statement}"));
    }
}

/// Holds `engine`'s lock for the rest of a test: every test in this file that
/// talks to an engine takes it first, so two of them never run DDL against
/// the same server at once.
///
/// Cargo runs one test binary at a time but the tests *inside* it in parallel,
/// and these tests are DDL from start to finish. YugabyteDB refuses concurrent
/// DDL outright — the constraint sweep's setup failed 4 of 4 full-file runs
/// with "could not serialize access due to concurrent update" while the
/// operation sequence ran beside it, and passed alone. The other engines took
/// it, but nothing here is worth a race: the lock costs a few seconds of
/// parallelism, per engine, inside one file. A tokio mutex, since the guard is
/// held across awaits and each `#[tokio::test]` has a runtime of its own.
async fn serialized(engine: &'static str) -> tokio::sync::OwnedMutexGuard<()> {
    type Locks = std::sync::Mutex<HashMap<&'static str, Arc<tokio::sync::Mutex<()>>>>;
    static LOCKS: OnceLock<Locks> = OnceLock::new();
    let lock = LOCKS
        .get_or_init(Default::default)
        .lock()
        .unwrap()
        .entry(engine)
        .or_default()
        .clone();
    lock.lock_owned().await
}

fn url(var: &str) -> Option<String> {
    match std::env::var(var) {
        Ok(url) => Some(url),
        Err(_) => {
            eprintln!("skipping schema-edit test: {var} not set");
            None
        }
    }
}

/// Opens `url` as Postgres, seeds the fixture, and runs the sequence.
async fn postgres_report(url: &str, name: &str, flavor: Flavor) -> Report {
    let pool = DbPool::open_postgres(url).await.unwrap();
    seed(&pool, name, flavor).await;
    let report = exercise(&pool, name, flavor).await;
    pool.close().await;
    report
}

#[tokio::test]
async fn postgres_applies_every_operation() {
    let _engine = serialized("HUBRO_PG_TEST_URL").await;
    let Some(url) = common::pg_test_url().await else {
        return;
    };
    assert_all_applied(
        &postgres_report(&url, "se_pg", Flavor::ORDINARY).await,
        "PostgreSQL",
    );
}

#[tokio::test]
async fn timescale_applies_every_operation() {
    let _engine = serialized("HUBRO_TIMESCALE_TEST_URL").await;
    let Some(url) = url("HUBRO_TIMESCALE_TEST_URL") else {
        return;
    };
    assert_all_applied(
        &postgres_report(&url, "se_ts", Flavor::ORDINARY).await,
        "TimescaleDB",
    );
}

#[tokio::test]
async fn citus_applies_every_operation() {
    let _engine = serialized("HUBRO_CITUS_TEST_URL").await;
    let Some(url) = url("HUBRO_CITUS_TEST_URL") else {
        return;
    };
    assert_all_applied(
        &postgres_report(&url, "se_citus", Flavor::ORDINARY).await,
        "Citus",
    );
}

#[tokio::test]
async fn cockroach_applies_every_operation() {
    let _engine = serialized("HUBRO_CRDB_TEST_URL").await;
    let Some(url) = url("HUBRO_CRDB_TEST_URL") else {
        return;
    };
    assert_all_applied(
        &postgres_report(&url, "se_crdb", Flavor::ORDINARY).await,
        "CockroachDB",
    );
}

#[tokio::test]
async fn yugabyte_applies_every_operation() {
    let _engine = serialized("HUBRO_YUGABYTE_TEST_URL").await;
    let Some(url) = url("HUBRO_YUGABYTE_TEST_URL") else {
        return;
    };
    assert_all_applied(
        &postgres_report(&url, "se_yb", Flavor::ORDINARY).await,
        "YugabyteDB",
    );
}

// ---------------------------------------------------------------------------
// SQL Server
// ---------------------------------------------------------------------------

#[tokio::test]
async fn sqlserver_applies_every_operation() {
    let _engine = serialized("HUBRO_MSSQL_TEST_URL").await;
    let Some(url) = common::mssql_test_url().await else {
        return;
    };
    let pool = DbPool::open_mssql(&url).await.unwrap();
    seed(&pool, "se_mssql", Flavor::ORDINARY).await;

    let report = exercise(&pool, "se_mssql", Flavor::ORDINARY).await;
    assert_all_applied(&report, "SQL Server");

    pool.close().await;
}

#[tokio::test]
async fn sqlserver_renames_through_sp_rename() {
    // The statement that differs most from the other dialects, checked
    // end to end: sp_rename takes its target inside a *string*, so the
    // bracket quoting is load-bearing in a way ordinary identifier quoting
    // is not.
    let _engine = serialized("HUBRO_MSSQL_TEST_URL").await;
    let Some(url) = common::mssql_test_url().await else {
        return;
    };
    let pool = DbPool::open_mssql(&url).await.unwrap();
    seed(&pool, "se_rename", Flavor::ORDINARY).await;
    let tables = pool.introspect().await.unwrap();
    let meta = table(&tables, "se_rename").clone();

    let sql = schema_op_sql(
        Dialect::SqlServer,
        &meta,
        &SchemaOp::RenameTable {
            new_name: "se_renamed".into(),
        },
    );
    assert!(
        sql.starts_with("EXEC sp_rename '[dbo].[se_rename]'"),
        "{sql}"
    );
    apply(
        &pool,
        &meta,
        &SchemaOp::RenameTable {
            new_name: "se_renamed".into(),
        },
    )
    .await
    .unwrap();

    let tables = pool.introspect().await.unwrap();
    assert!(find(&tables, "se_renamed").is_some());
    assert!(find(&tables, "se_rename").is_none());

    let _ = pool.query("DROP TABLE se_renamed").await;
    pool.close().await;
}

// ---------------------------------------------------------------------------
// Indexes a constraint owns, on the engines that have such constraints.
// Materialize and RisingWave have none (an empty `pg_constraint`); their drop of
// a user-made index is covered by their sequences below.
// ---------------------------------------------------------------------------

/// PostgreSQL, TimescaleDB and Citus also have EXCLUDE constraints, whose
/// index is as undroppable as the other two.
async fn assert_postgres_constraint_sweep(url: &str, name: &str, engine: &str) {
    let pool = DbPool::open_postgres(url).await.unwrap();
    let setup = constraint_fixture(
        name,
        "varchar(40)",
        ", span int4range, EXCLUDE USING gist (span WITH &&)",
    );
    let sweep = constraint_sweep(&pool, name, &setup, "because constraint").await;
    assert_eq!(
        sweep.refused,
        sorted(vec![
            PRIMARY_KEY_INDEX,
            UNIQUE_CONSTRAINT_INDEX,
            UNIQUE_CONSTRAINT_INDEX,
            EXCLUSION_CONSTRAINT_INDEX,
        ]),
        "{engine}"
    );
    assert_eq!(sweep.dropped, 2, "{engine}: {sweep:?}");
    assert!(sweep.offered_but_refused.is_empty(), "{engine}: {sweep:?}");
    teardown(&pool, name).await;
    pool.close().await;
}

#[tokio::test]
async fn postgres_an_index_built_for_a_constraint_is_not_offered_for_drop() {
    let _engine = serialized("HUBRO_PG_TEST_URL").await;
    let Some(url) = common::pg_test_url().await else {
        return;
    };
    assert_postgres_constraint_sweep(&url, "ci_pg", "PostgreSQL").await;
}

#[tokio::test]
async fn timescale_an_index_built_for_a_constraint_is_not_offered_for_drop() {
    let _engine = serialized("HUBRO_TIMESCALE_TEST_URL").await;
    let Some(url) = url("HUBRO_TIMESCALE_TEST_URL") else {
        return;
    };
    assert_postgres_constraint_sweep(&url, "ci_ts", "TimescaleDB").await;
}

#[tokio::test]
async fn citus_an_index_built_for_a_constraint_is_not_offered_for_drop() {
    let _engine = serialized("HUBRO_CITUS_TEST_URL").await;
    let Some(url) = url("HUBRO_CITUS_TEST_URL") else {
        return;
    };
    assert_postgres_constraint_sweep(&url, "ci_citus", "Citus").await;
}

#[tokio::test]
async fn yugabyte_an_index_built_for_a_constraint_is_not_offered_for_drop() {
    let _engine = serialized("HUBRO_YUGABYTE_TEST_URL").await;
    let Some(url) = url("HUBRO_YUGABYTE_TEST_URL") else {
        return;
    };
    // No EXCLUDE: "EXCLUDE constraint not supported yet".
    let pool = DbPool::open_postgres(&url).await.unwrap();
    let setup = constraint_fixture("ci_yb", "varchar(40)", "");
    let sweep = constraint_sweep(&pool, "ci_yb", &setup, "because constraint").await;
    assert_eq!(
        sweep.refused,
        sorted(vec![
            PRIMARY_KEY_INDEX,
            UNIQUE_CONSTRAINT_INDEX,
            UNIQUE_CONSTRAINT_INDEX
        ])
    );
    assert_eq!(sweep.dropped, 2, "{sweep:?}");
    assert!(sweep.offered_but_refused.is_empty(), "{sweep:?}");
    teardown(&pool, "ci_yb").await;
    pool.close().await;
}

/// CockroachDB lists a `CREATE UNIQUE INDEX` in `pg_constraint` exactly as it
/// lists a UNIQUE constraint, so hubro claims only its primary key (see
/// `pg_index_constraint`). This pins the cost of that: the two UNIQUE
/// constraints' indexes are offered and the engine refuses them — a refusal the
/// user reads, never a statement accepted while nothing changes — while the
/// unique index the user made really does drop.
#[tokio::test]
async fn cockroach_offers_a_unique_constraints_index_and_the_engine_refuses_it() {
    let _engine = serialized("HUBRO_CRDB_TEST_URL").await;
    let Some(url) = url("HUBRO_CRDB_TEST_URL") else {
        return;
    };
    let pool = DbPool::open_postgres(&url).await.unwrap();
    let setup = constraint_fixture("ci_crdb", "varchar(40)", "");
    let sweep = constraint_sweep(&pool, "ci_crdb", &setup, "cannot drop the primary index").await;
    assert_eq!(sweep.refused, vec![PRIMARY_KEY_INDEX]);
    assert_eq!(sweep.dropped, 2, "{sweep:?}");
    assert_eq!(sweep.offered_but_refused.len(), 2, "{sweep:?}");
    for err in &sweep.offered_but_refused {
        assert!(err.contains("is in use as unique constraint"), "{err}");
    }
    teardown(&pool, "ci_crdb").await;
    pool.close().await;
}

#[tokio::test]
async fn sqlserver_an_index_built_for_a_constraint_is_not_offered_for_drop() {
    let _engine = serialized("HUBRO_MSSQL_TEST_URL").await;
    let Some(url) = common::mssql_test_url().await else {
        return;
    };
    let pool = DbPool::open_mssql(&url).await.unwrap();
    let setup = constraint_fixture("ci_mssql", "nvarchar(40)", "");
    let sweep = constraint_sweep(&pool, "ci_mssql", &setup, "constraint enforcement").await;
    assert_eq!(
        sweep.refused,
        sorted(vec![
            PRIMARY_KEY_INDEX,
            UNIQUE_CONSTRAINT_INDEX,
            UNIQUE_CONSTRAINT_INDEX
        ])
    );
    assert_eq!(sweep.dropped, 2, "{sweep:?}");
    assert!(sweep.offered_but_refused.is_empty(), "{sweep:?}");
    teardown(&pool, "ci_mssql").await;
    pool.close().await;
}

// ---------------------------------------------------------------------------
// Materialize and RisingWave — the two that do not take the whole set
// ---------------------------------------------------------------------------

/// Materialize takes four of the seven. What it refuses, it refuses loudly.
///
/// `ADD COLUMN` is gated behind a feature flag that is off by default
/// ("Enable ALTER TABLE ... ADD COLUMN ... is not available"), `RENAME COLUMN`
/// is not implemented, and there is no `TRUNCATE` in its grammar at all. Its
/// indexes *are* real to introspection, which is why create/drop index count as
/// applied here rather than as accepted-and-ignored.
#[tokio::test]
async fn materialize_applies_what_its_ddl_supports() {
    let _engine = serialized("HUBRO_MATERIALIZE_TEST_URL").await;
    let Some(url) = url("HUBRO_MATERIALIZE_TEST_URL") else {
        return;
    };
    let report = postgres_report(
        &url,
        "se_mz",
        Flavor {
            text_type: "varchar(40)",
            // Materialize rejects `CREATE TABLE` with a primary key or a
            // unique constraint outright.
            primary_key: false,
        },
    )
    .await;
    assert_supported_except(
        &report,
        "Materialize",
        &["add column", "rename column", "truncate"],
    );
}

/// RisingWave takes five of the seven: it has `ADD COLUMN`, but `RENAME COLUMN`
/// and `TRUNCATE` are both "not yet implemented" and say so.
#[tokio::test]
async fn risingwave_applies_what_its_ddl_supports() {
    let _engine = serialized("HUBRO_RISINGWAVE_TEST_URL").await;
    let Some(url) = url("HUBRO_RISINGWAVE_TEST_URL") else {
        return;
    };
    let report = postgres_report(
        &url,
        "se_rw",
        Flavor {
            // RisingWave's parser has no parameterised `varchar(n)`.
            text_type: "varchar",
            primary_key: true,
        },
    )
    .await;
    assert_supported_except(&report, "RisingWave", &["rename column", "truncate"]);
}

/// What the explicit `NULL` in SQL Server's `ADD` is worth, measured.
///
/// It went in to defend against `ANSI_NULL_DFLT_OFF`, on the documented rule
/// that a column added without stated nullability is then `NOT NULL` — which
/// would fail on a table that already has rows. **That does not reproduce.**
/// With the setting flipped, SQL Server 2022 (16.0.4265) makes a bare
/// `CREATE TABLE` column `NOT NULL` and still makes a bare
/// `ALTER TABLE … ADD` column nullable.
///
/// So this pins the two things that are true: the session default reaches
/// `CREATE TABLE` (which is why the setting is not simply inert), and hubro's
/// generated `ADD` produces a nullable column under either default. Both
/// statements go in one batch because the `SET` is session-scoped and the pool
/// hands out connections per statement.
///
/// The alternative was to leave a comment asserting a behaviour no test
/// exercises — which is how a justification outlives the thing that made it
/// true.
#[tokio::test]
async fn sqlserver_add_column_ignores_the_session_null_default() {
    let _engine = serialized("HUBRO_MSSQL_TEST_URL").await;
    let Some(url) = common::mssql_test_url().await else {
        return;
    };
    let pool = DbPool::open_mssql(&url).await.unwrap();
    let _ = pool.query("DROP TABLE se_nulls").await;

    // The setting is not inert: it reaches CREATE TABLE, where a bare column
    // does come out NOT NULL.
    pool.query(
        "SET ANSI_NULL_DFLT_ON OFF; SET ANSI_NULL_DFLT_OFF ON; \
         CREATE TABLE se_nulls (id int, name varchar(40) NOT NULL);",
    )
    .await
    .unwrap();
    let tables = pool.introspect().await.unwrap();
    let meta = table(&tables, "se_nulls").clone();
    let id = meta.columns.iter().find(|c| c.name == "id").unwrap();
    assert!(
        !id.nullable,
        "the session default did not reach CREATE TABLE, so this test proves nothing"
    );

    // ALTER TABLE … ADD, however, is nullable under that same setting whether
    // or not the keyword is there — so the keyword is a statement of intent,
    // not a defence.
    for statement in [
        "ALTER TABLE \"dbo\".\"se_nulls\" ADD \"bare\" int;".to_string(),
        schema_op_sql(
            Dialect::SqlServer,
            &meta,
            &SchemaOp::AddColumn {
                name: "explicit".into(),
                type_name: "int".into(),
            },
        ),
    ] {
        pool.query(&format!(
            "SET ANSI_NULL_DFLT_ON OFF; SET ANSI_NULL_DFLT_OFF ON; {statement}"
        ))
        .await
        .unwrap_or_else(|e| panic!("{statement} was refused: {e}"));
    }
    let tables = pool.introspect().await.unwrap();
    for name in ["bare", "explicit"] {
        let column = table(&tables, "se_nulls")
            .columns
            .iter()
            .find(|c| c.name == name)
            .unwrap_or_else(|| panic!("{name} is missing"));
        assert!(column.nullable, "{name} came out NOT NULL");
    }

    let _ = pool.query("DROP TABLE se_nulls").await;
    pool.close().await;
}
