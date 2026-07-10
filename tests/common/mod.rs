//! Shared test support: builds SQLite fixture databases in temp directories.
//!
//! Fixtures are generated at test setup time — no binary files are checked
//! in. Each [`FixtureDb`] owns its temp dir, so the database file lives until
//! the fixture is dropped.

// Each integration-test binary compiles its own copy of this module and uses
// only a subset of it, so unused-item lints would fire spuriously.
#![allow(dead_code)]

use std::path::{Path, PathBuf};

use dataview::db::DbPool;
use sqlx::sqlite::SqliteConnectOptions;
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
    _dir: tempfile::TempDir,
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
        FixtureDb { _dir: dir, path }
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
