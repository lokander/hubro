//! Persisted query history, one SQLite file for the whole app
//! (`$XDG_DATA_HOME/hubro/history.db`).
//!
//! Executed scripts are recorded per connection locator (file path / URL)
//! with a timestamp and their overall success/failure. The store keeps the
//! newest [`HISTORY_CAP`] entries per locator and a persisted opt-out flag.
//!
//! The history database is only ever touched through this module's own
//! private pool — it is never opened as a user connection tab — so queries
//! the app runs against history are naturally never recorded into it.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use sqlx::sqlite::{SqliteConnectOptions, SqlitePool, SqlitePoolOptions};
use sqlx::Row as _;

/// Maximum entries kept per connection locator; older ones are pruned on
/// insert.
pub const HISTORY_CAP: i64 = 500;

/// One recorded script run.
#[derive(Debug, Clone, PartialEq)]
pub struct HistoryEntry {
    pub id: i64,
    /// The full script text as the user ran it.
    pub sql: String,
    /// Unix seconds.
    pub executed_at: i64,
    pub success: bool,
    /// The first error when the run failed.
    pub error: Option<String>,
}

/// Default location: `$XDG_DATA_HOME/hubro/history.db`.
pub fn default_history_path() -> Option<PathBuf> {
    Some(dirs::data_dir()?.join("hubro").join("history.db"))
}

/// Handle to the history database. Cheap to clone (wraps a pool).
#[derive(Debug, Clone)]
pub struct HistoryStore {
    pool: SqlitePool,
}

impl HistoryStore {
    /// Opens (creating if missing) the history database at its default
    /// location.
    pub async fn open() -> Result<Self, String> {
        let path = default_history_path().ok_or_else(|| "no data directory found".to_string())?;
        Self::open_at(&path).await
    }

    /// Opens the history database at an explicit path (used by tests).
    /// Unlike user databases, the file is created when missing — this store
    /// is app-owned, not user data.
    pub async fn open_at(path: &Path) -> Result<Self, String> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("creating {}: {e}", parent.display()))?;
        }
        let options = SqliteConnectOptions::new()
            .filename(path)
            .create_if_missing(true);
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(options)
            .await
            .map_err(|e| format!("opening {}: {e}", path.display()))?;
        for statement in [
            "CREATE TABLE IF NOT EXISTS entries (\
                 id INTEGER PRIMARY KEY, \
                 locator TEXT NOT NULL, \
                 sql TEXT NOT NULL, \
                 executed_at INTEGER NOT NULL, \
                 success INTEGER NOT NULL, \
                 error TEXT\
             )",
            "CREATE INDEX IF NOT EXISTS entries_by_locator ON entries(locator, id)",
            "CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL)",
        ] {
            sqlx::query(statement)
                .execute(&pool)
                .await
                .map_err(|e| format!("initializing history schema: {e}"))?;
        }
        Ok(Self { pool })
    }

    /// Whether new runs are recorded. Defaults to true when never set.
    pub async fn recording_enabled(&self) -> Result<bool, String> {
        let row = sqlx::query("SELECT value FROM meta WHERE key = 'recording'")
            .fetch_optional(&self.pool)
            .await
            .map_err(|e| e.to_string())?;
        Ok(match row {
            Some(row) => {
                row.try_get::<String, _>("value")
                    .map_err(|e| e.to_string())?
                    != "off"
            }
            None => true,
        })
    }

    /// Persists the opt-out flag.
    pub async fn set_recording(&self, enabled: bool) -> Result<(), String> {
        sqlx::query(
            "INSERT INTO meta (key, value) VALUES ('recording', ?) \
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        )
        .bind(if enabled { "on" } else { "off" })
        .execute(&self.pool)
        .await
        .map_err(|e| e.to_string())?;
        Ok(())
    }

    /// Records one script run, then prunes the locator's history beyond the
    /// newest [`HISTORY_CAP`] entries. Returns `false` (recording nothing)
    /// when recording is disabled.
    pub async fn record(
        &self,
        locator: &str,
        sql: &str,
        success: bool,
        error: Option<&str>,
    ) -> Result<bool, String> {
        if !self.recording_enabled().await? {
            return Ok(false);
        }
        let executed_at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        sqlx::query(
            "INSERT INTO entries (locator, sql, executed_at, success, error) \
             VALUES (?, ?, ?, ?, ?)",
        )
        .bind(locator)
        .bind(sql)
        .bind(executed_at)
        .bind(success as i64)
        .bind(error)
        .execute(&self.pool)
        .await
        .map_err(|e| e.to_string())?;
        sqlx::query(
            "DELETE FROM entries WHERE locator = ? AND id NOT IN (\
                 SELECT id FROM entries WHERE locator = ? ORDER BY id DESC LIMIT ?\
             )",
        )
        .bind(locator)
        .bind(locator)
        .bind(HISTORY_CAP)
        .execute(&self.pool)
        .await
        .map_err(|e| e.to_string())?;
        Ok(true)
    }

    /// Lists one locator's entries newest-first, optionally filtered by a
    /// case-insensitive substring match on the SQL text.
    pub async fn list(
        &self,
        locator: &str,
        search: Option<&str>,
        limit: i64,
    ) -> Result<Vec<HistoryEntry>, String> {
        let filtered = search.map(str::trim).filter(|s| !s.is_empty());
        let mut query = String::from(
            "SELECT id, sql, executed_at, success, error FROM entries WHERE locator = ?",
        );
        if filtered.is_some() {
            query.push_str(" AND sql LIKE ? ESCAPE '\\'");
        }
        query.push_str(" ORDER BY id DESC LIMIT ?");
        let mut prepared = sqlx::query(&query).bind(locator);
        if let Some(needle) = filtered {
            prepared = prepared.bind(format!("%{}%", escape_like(needle)));
        }
        let rows = prepared
            .bind(limit)
            .fetch_all(&self.pool)
            .await
            .map_err(|e| e.to_string())?;
        let mut entries = Vec::with_capacity(rows.len());
        for row in rows {
            entries.push(HistoryEntry {
                id: row.try_get("id").map_err(|e| e.to_string())?,
                sql: row.try_get("sql").map_err(|e| e.to_string())?,
                executed_at: row.try_get("executed_at").map_err(|e| e.to_string())?,
                success: row
                    .try_get::<i64, _>("success")
                    .map_err(|e| e.to_string())?
                    != 0,
                error: row.try_get("error").map_err(|e| e.to_string())?,
            });
        }
        Ok(entries)
    }

    /// Deletes all entries for one locator.
    pub async fn clear(&self, locator: &str) -> Result<(), String> {
        sqlx::query("DELETE FROM entries WHERE locator = ?")
            .bind(locator)
            .execute(&self.pool)
            .await
            .map_err(|e| e.to_string())?;
        Ok(())
    }
}

/// Escapes LIKE wildcards in user input so "50%" matches literally (same
/// scheme as the data-grid filter in `db::page`).
fn escape_like(needle: &str) -> String {
    needle
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escape_like_escapes_wildcards_and_backslashes() {
        assert_eq!(escape_like("50%_x"), "50\\%\\_x");
        assert_eq!(escape_like("a\\b"), "a\\\\b");
        assert_eq!(escape_like("plain"), "plain");
    }
}
