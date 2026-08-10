//! Guards the README's "Supported databases" matrix (FRE-96) against the code
//! it describes.
//!
//! The matrix is prose, and prose rots. What rots first is coverage: an engine
//! gets recognised in `PgFlavor`, its verification lands, and the published
//! table still says hubro handles three databases. The whole promise of the
//! matrix is that you do not have to check whether your database is handled —
//! which only holds if nobody can add an engine without saying so.
//!
//! So this asserts that every engine the code can *name* has a row. The
//! mapping is declared once through a macro that emits both the list to
//! iterate and an exhaustive `match` to typecheck, so a new [`PgFlavor`]
//! variant stops this file compiling until someone decides what it is called
//! in the table — a better moment to be asked than after release. [`Dialect`]
//! is matched exhaustively for the same reason.
//!
//! Deliberately not asserted: the Browse/Edit/Script cells. They restate what
//! `backend_capabilities` and `TableAccess::resolve` decide, but a *server*'s
//! capabilities are only knowable with that server running, and this file has
//! to pass in CI with no containers at all. The per-engine suites check the
//! declarations against the real thing; this checks that the engine is
//! mentioned at all.

use hubro::db::{Dialect, PgFlavor};

const README: &str = include_str!("../README.md");

/// The heading the table lives under, and the anchor the rest of the README
/// links to. Pinned here because two `Features` bullets and the intro link to
/// `#supported-databases`, and renaming the heading would silently break them.
const HEADING: &str = "## Supported databases";

/// Declares the flavor-to-row-name mapping *once*, as both a list to iterate
/// and an exhaustive `match` to typecheck.
///
/// The two have to come from one source or the guard has a hole: with a
/// hand-written array beside a hand-written match, adding a [`PgFlavor`]
/// variant and satisfying only the compiler — which asks about the match and
/// not the array — leaves the new engine unlisted and every test green. That
/// is precisely the drift this file exists to catch, so the array is generated
/// rather than maintained.
macro_rules! flavor_rows {
    ($($variant:ident => $name:literal,)+) => {
        /// Every flavor the detection can return, with its row name.
        const FLAVORS: &[(PgFlavor, &str)] = &[$((PgFlavor::$variant, $name),)+];

        /// Never called: it exists so the compiler rejects this file until a
        /// new variant is added to the list above.
        #[allow(dead_code)]
        fn exhaustive(flavor: PgFlavor) -> &'static str {
            match flavor {
                $(PgFlavor::$variant => $name,)+
            }
        }
    };
}

flavor_rows! {
    Postgres => "PostgreSQL",
    CockroachDB => "CockroachDB",
    Yugabyte => "YugabyteDB",
    Materialize => "Materialize",
    RisingWave => "RisingWave",
}

fn dialect_row(dialect: Dialect) -> &'static str {
    match dialect {
        Dialect::Sqlite => "SQLite",
        Dialect::Postgres => "PostgreSQL",
        Dialect::SqlServer => "SQL Server",
    }
}

/// The table's body rows, as `Vec<Vec<cell>>`.
fn matrix_rows() -> Vec<Vec<String>> {
    let section = README
        .split_once(HEADING)
        .unwrap_or_else(|| panic!("README has no `{HEADING}` heading"))
        .1;
    section
        .lines()
        .take_while(|line| !line.starts_with("## "))
        .filter(|line| line.starts_with('|'))
        // The header row and its `| --- |` separator are not data.
        .filter(|line| !line.contains("---") && !line.contains("Via backend"))
        .map(|line| {
            line.trim_matches('|')
                .split('|')
                .map(|cell| cell.trim().to_string())
                .collect()
        })
        .collect()
}

#[test]
fn every_engine_the_code_names_has_a_row_in_the_matrix() {
    let rows = matrix_rows();
    assert!(
        rows.len() >= FLAVORS.len(),
        "the matrix has {} rows, fewer than the engines that can be detected",
        rows.len()
    );
    let engines: Vec<&str> = rows.iter().map(|row| row[0].as_str()).collect();

    for (flavor, name) in FLAVORS {
        assert!(
            engines.contains(name),
            "{flavor:?} is detected by hubro but has no row in the support matrix — \
             engines: {engines:?}"
        );
    }
    for dialect in [Dialect::Sqlite, Dialect::Postgres, Dialect::SqlServer] {
        let name = dialect_row(dialect);
        assert!(
            engines.contains(&name),
            "{dialect:?} is a backend but has no row in the support matrix"
        );
    }
}

#[test]
fn every_row_records_a_version_and_all_three_capability_columns() {
    for row in matrix_rows() {
        let engine = &row[0];
        assert_eq!(
            row.len(),
            7,
            "{engine}: expected Engine | Via backend | Verified version | Browse | \
             Edit | Script | Notes, got {} cells",
            row.len()
        );
        // The rule the issue put first: a version that is not a version is the
        // "supports PostgreSQL" claim nobody can check.
        let version = &row[2];
        assert!(
            version.chars().any(|c| c.is_ascii_digit()),
            "{engine}: version cell {version:?} records no version"
        );
        assert!(
            !version.contains("latest"),
            "{engine}: {version:?} is an image tag, not the version that ran"
        );
        for (column, cell) in ["Browse", "Edit", "Script"].iter().zip(&row[3..6]) {
            assert!(!cell.is_empty(), "{engine}: {column} cell is empty");
        }
    }
}

#[test]
fn the_engines_with_their_own_test_file_are_all_in_the_matrix() {
    // Catches the case the flavor check cannot: TimescaleDB and Citus are
    // extensions on stock PostgreSQL, so they are verified separately but
    // share `PgFlavor::Postgres` and would otherwise go unmentioned.
    let engines: Vec<String> = matrix_rows()
        .into_iter()
        .map(|row| row[0].clone())
        .collect();
    for (file, engine) in [
        ("tests/db_timescale.rs", "TimescaleDB"),
        ("tests/db_citus.rs", "Citus"),
    ] {
        assert!(
            std::path::Path::new(file).exists(),
            "{file} is gone — has this engine's verification been dropped?"
        );
        assert!(
            engines.contains(&engine.to_string()),
            "{engine} has {file} but no row in the support matrix"
        );
    }
}
