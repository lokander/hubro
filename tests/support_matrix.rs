//! Guards the README's "Supported databases" matrix against the code
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
    body_rows(section)
}

/// The body rows of the first table in `section`: the run of `|` lines that
/// starts it, minus the header row and the separator under it.
///
/// Told apart by position, not content. The first version dropped every line
/// containing `---` or `Via backend`, so a row whose Notes said either one
/// vanished from the checks — the opposite of what a guard on the table
/// should do with new text. A Markdown table is a header, then a separator
/// whose every cell is dashes (optionally colon-aligned), then the body, so
/// that is what is checked: a table without that separator is not one this
/// can read, and says so.
fn body_rows(section: &str) -> Vec<Vec<String>> {
    let mut table = section
        .lines()
        .take_while(|line| !line.starts_with("## "))
        .skip_while(|line| !line.starts_with('|'))
        .take_while(|line| line.starts_with('|'))
        .map(cells);
    let header = table.next().expect("the section has no table");
    let separator = table.next().unwrap_or_default();
    assert!(
        !separator.is_empty() && separator.iter().all(|cell| is_separator_cell(cell)),
        "the row under the header {header:?} is {separator:?}, not a `| --- |` separator"
    );
    assert_eq!(
        separator.len(),
        header.len(),
        "the separator has a different number of cells from the header"
    );
    table.collect()
}

fn cells(line: &str) -> Vec<String> {
    line.trim()
        .trim_matches('|')
        .split('|')
        .map(|cell| cell.trim().to_string())
        .collect()
}

/// `---`, `:---`, `---:` or `:---:`: three or more dashes, colons at the ends
/// only.
fn is_separator_cell(cell: &str) -> bool {
    let dashes = cell.strip_prefix(':').unwrap_or(cell);
    let dashes = dashes.strip_suffix(':').unwrap_or(dashes);
    dashes.len() >= 3 && dashes.bytes().all(|b| b == b'-')
}

/// Whether a Verified version cell records a version that ran: some word in
/// it, brackets aside, is a dotted number — `17.10`, `v26.2.5`,
/// `2026.1.0.1` — optionally with a packaging revision, as in Citus's
/// `14.1-1`.
///
/// What it refuses is an image tag standing in for one: `17-alpine`, `pg17`,
/// `latest`, `2022-latest`. A bare major number is refused too, because "17"
/// is the tag rather than what `SELECT version()` reported; SQL Server's row
/// names its year *and* the build that ran, and the build is what passes.
fn records_a_version(cell: &str) -> bool {
    cell.split_whitespace()
        .map(|word| word.trim_matches(|c| c == '(' || c == ')' || c == ','))
        .any(|word| {
            let word = word.strip_prefix('v').unwrap_or(word);
            let (number, revision) = word.split_once('-').unwrap_or((word, "0"));
            let parts: Vec<&str> = number.split('.').collect();
            parts.len() >= 2 && parts.iter().all(|part| is_digits(part)) && is_digits(revision)
        })
}

fn is_digits(text: &str) -> bool {
    !text.is_empty() && text.bytes().all(|b| b.is_ascii_digit())
}

#[test]
fn the_table_is_read_by_its_shape_not_its_words() {
    let section = "intro\n\n\
        | Engine | Via backend | Notes |\n\
        | :--- | ---: | :---: |\n\
        | A | x | Has --- in its notes. |\n\
        | B | y | Mentions Via backend. |\n\
        \n\
        | Other | table |\n\
        | --- | --- |\n\
        | C | z |\n";
    let rows = body_rows(section);
    let engines: Vec<&str> = rows.iter().map(|row| row[0].as_str()).collect();
    assert_eq!(engines, ["A", "B"]);
    assert_eq!(rows[0][2], "Has --- in its notes.");

    for (no_separator, why) in [
        (
            "| Engine | Notes |\n| A | x |\n",
            "a body row under the header",
        ),
        ("| Engine | Notes |\n| -- | --- |\n", "two dashes"),
        (
            "| Engine | Notes |\n| --- |\n",
            "fewer cells than the header",
        ),
        ("| Engine | Notes |\n", "nothing under the header"),
    ] {
        assert!(
            std::panic::catch_unwind(|| body_rows(no_separator)).is_err(),
            "{why} was read as a separator"
        );
    }
}

#[test]
fn a_version_is_a_dotted_number_not_an_image_tag() {
    for version in [
        "3.46.0",
        "17.10",
        "2022 (16.0.4265.3)",
        "2.29.1 (on PostgreSQL 17.10)",
        "14.1-1 (on PostgreSQL 18.4)",
        "v26.2.5",
        "2026.1.0.1 (PostgreSQL 15.12)",
    ] {
        assert!(records_a_version(version), "{version:?} was refused");
    }
    for tag in [
        "17",
        "17-alpine",
        "17.6-alpine",
        "pg17",
        "latest",
        "2022-latest",
        "17.x",
        "1.",
        ".5",
        "",
    ] {
        assert!(!records_a_version(tag), "{tag:?} passed as a version");
    }
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

/// Everything wrong with one body row, one line each — empty when the row has
/// all seven cells, a version that ran and every capability filled in.
///
/// A function over the row rather than asserts over the README, so the rule
/// can be shown failing on rows the README does not have (and should not).
fn row_problems(row: &[String]) -> Vec<String> {
    let engine = row.first().map_or("(empty row)", String::as_str);
    if row.len() != 7 {
        return vec![format!(
            "{engine}: expected Engine | Via backend | Verified version | Browse | \
             Edit | Script | Notes, got {} cells",
            row.len()
        )];
    }
    let mut problems = Vec::new();
    // The rule the issue put first: a version that is not a version is the
    // "supports PostgreSQL" claim nobody can check.
    let version = &row[2];
    if !records_a_version(version) {
        problems.push(format!(
            "{engine}: version cell {version:?} records no dotted version number"
        ));
    }
    if version.contains("latest") {
        problems.push(format!(
            "{engine}: {version:?} is an image tag, not the version that ran"
        ));
    }
    for (column, cell) in ["Browse", "Edit", "Script"].iter().zip(&row[3..6]) {
        if cell.is_empty() {
            problems.push(format!("{engine}: {column} cell is empty"));
        }
    }
    problems
}

#[test]
fn every_row_records_a_version_and_all_three_capability_columns() {
    let problems: Vec<String> = matrix_rows()
        .iter()
        .flat_map(|row| row_problems(row))
        .collect();
    assert!(problems.is_empty(), "{}", problems.join("\n"));
}

#[test]
fn a_row_with_a_tag_for_a_version_or_a_missing_cell_is_refused() {
    let row = |cells: &[&str]| cells.iter().map(|c| c.to_string()).collect::<Vec<_>>();
    let good = row(&["E", "Postgres", "17.10", "Yes", "Yes", "Atomic", "n"]);
    assert_eq!(row_problems(&good), Vec::<String>::new());
    for bad in [
        row(&["E", "Postgres", "17-alpine", "Yes", "Yes", "Atomic", "n"]),
        row(&["E", "Postgres", "pg17", "Yes", "Yes", "Atomic", "n"]),
        row(&[
            "E",
            "Postgres",
            "17.10 (latest)",
            "Yes",
            "Yes",
            "Atomic",
            "n",
        ]),
        row(&["E", "Postgres", "17.10", "Yes", "", "Atomic", "n"]),
        row(&["E", "Postgres", "17.10", "Yes", "Yes", "Atomic"]),
    ] {
        assert!(!row_problems(&bad).is_empty(), "{bad:?} passed");
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
