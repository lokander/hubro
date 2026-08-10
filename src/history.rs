//! Persisted query history and saved queries, one SQLite file for the whole
//! app (`$XDG_DATA_HOME/hubro/history.db`).
//!
//! Executed scripts are recorded per connection locator (file path / URL)
//! with a timestamp and their overall success/failure. The store keeps the
//! newest [`HISTORY_CAP`] entries per locator and a persisted opt-out flag.
//!
//! Named queries the user chose to keep (FRE-113) live in a second table in
//! the same file, so there is one app-owned database rather than two. They
//! are *not* history: the recording opt-out does not gate them, the
//! [`HISTORY_CAP`] pruning never reaches them, and clearing a connection's
//! history leaves them alone — a deliberate save is not a side effect of
//! running something.
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

/// One saved query (FRE-113): editor contents the user named and kept.
#[derive(Debug, Clone, PartialEq)]
pub struct SavedQuery {
    pub id: i64,
    /// Unique within its scope; what the list is sorted and searched by.
    pub name: String,
    /// Why the query exists — the thing that makes it still make sense in
    /// three months. `None` when the user left it blank.
    pub description: Option<String>,
    pub sql: String,
    /// The connection this query belongs to, or `None` when it is global:
    /// offered on every connection, which is what utility snippets want and
    /// a query written against one schema does not.
    pub locator: Option<String>,
    /// Unix seconds of the last save under this name.
    pub updated_at: i64,
}

/// What a [`HistoryStore::save_query`] call did — the two cases the UI must
/// report differently, since one of them replaced something.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SaveOutcome {
    Created,
    /// A query of the same name in the same scope was overwritten.
    Replaced,
}

/// The stored `locator` of a global saved query.
///
/// A real locator is a canonical file path or a connection URL, so it is
/// never empty — which is what makes the empty string usable as the "no
/// connection" scope. Storing a sentinel rather than SQL `NULL` is what lets
/// `UNIQUE(locator, name)` police global names too: SQLite treats NULLs in a
/// unique index as distinct, so a nullable scope column would happily hold
/// ten globals called "row counts".
const GLOBAL_SCOPE: &str = "";

/// The stored scope for an optional locator. An empty locator is treated as
/// global rather than as a connection nobody can name, so the sentinel and
/// its absence can never disagree.
fn scope_of(locator: Option<&str>) -> &str {
    match locator {
        Some(locator) if !locator.is_empty() => locator,
        _ => GLOBAL_SCOPE,
    }
}

/// The [`SavedQuery::locator`] a stored scope decodes to.
fn locator_of(scope: String) -> Option<String> {
    (scope != GLOBAL_SCOPE).then_some(scope)
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
            // Saved queries (FRE-113). `locator` is the scope: the connection
            // the query belongs to, or GLOBAL_SCOPE for one offered
            // everywhere.
            "CREATE TABLE IF NOT EXISTS saved_queries (\
                 id INTEGER PRIMARY KEY, \
                 locator TEXT NOT NULL, \
                 name TEXT NOT NULL, \
                 description TEXT, \
                 sql TEXT NOT NULL, \
                 created_at INTEGER NOT NULL, \
                 updated_at INTEGER NOT NULL\
             )",
            // Names are unique per scope, which is what makes re-saving under
            // an existing name an update instead of a second entry nobody can
            // tell from the first.
            "CREATE UNIQUE INDEX IF NOT EXISTS saved_queries_by_scope \
             ON saved_queries(locator, name)",
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
        let executed_at = unix_now();
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

    /// Deletes all entries for one locator. Saved queries are a different
    /// table and are deliberately untouched — clearing what you happened to
    /// run must not delete what you chose to keep.
    pub async fn clear(&self, locator: &str) -> Result<(), String> {
        sqlx::query("DELETE FROM entries WHERE locator = ?")
            .bind(locator)
            .execute(&self.pool)
            .await
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    /// Saves editor contents under `name` (FRE-113), scoped to `locator` or
    /// global when that is `None`. Re-saving an existing name *in the same
    /// scope* overwrites it and reports [`SaveOutcome::Replaced`]; the same
    /// name under a different scope is a different query.
    ///
    /// Unlike [`Self::record`] this ignores the recording opt-out: that flag
    /// is about hubro recording what you ran behind your back, and this is
    /// something you asked for.
    pub async fn save_query(
        &self,
        name: &str,
        description: Option<&str>,
        sql: &str,
        locator: Option<&str>,
    ) -> Result<SaveOutcome, String> {
        let name = name.trim();
        if name.is_empty() {
            return Err("a saved query needs a name".to_string());
        }
        if sql.trim().is_empty() {
            return Err("there is nothing to save — the editor is empty".to_string());
        }
        let description = description.map(str::trim).filter(|d| !d.is_empty());
        let scope = scope_of(locator);
        let existing: Option<i64> =
            sqlx::query_scalar("SELECT id FROM saved_queries WHERE locator = ? AND name = ?")
                .bind(scope)
                .bind(name)
                .fetch_optional(&self.pool)
                .await
                .map_err(|e| e.to_string())?;
        let now = unix_now();
        sqlx::query(
            "INSERT INTO saved_queries (locator, name, description, sql, created_at, updated_at) \
             VALUES (?, ?, ?, ?, ?, ?) \
             ON CONFLICT(locator, name) DO UPDATE SET \
                 description = excluded.description, \
                 sql = excluded.sql, \
                 updated_at = excluded.updated_at",
        )
        .bind(scope)
        .bind(name)
        .bind(description)
        .bind(sql)
        .bind(now)
        .bind(now)
        .execute(&self.pool)
        .await
        .map_err(|e| e.to_string())?;
        Ok(match existing {
            Some(_) => SaveOutcome::Replaced,
            None => SaveOutcome::Created,
        })
    }

    /// The saved queries usable from one connection: its own plus every
    /// global one, its own first and each group by name. `search` is an
    /// optional case-insensitive substring, matched against the name, the
    /// description and the SQL — a query is as likely to be remembered by
    /// what it selects as by what it was called.
    pub async fn saved_queries(
        &self,
        locator: &str,
        search: Option<&str>,
    ) -> Result<Vec<SavedQuery>, String> {
        let filtered = search.map(str::trim).filter(|s| !s.is_empty());
        let mut query = String::from(
            "SELECT id, locator, name, description, sql, updated_at FROM saved_queries \
             WHERE (locator = ? OR locator = ?)",
        );
        if filtered.is_some() {
            query.push_str(
                " AND (name LIKE ? ESCAPE '\\' OR description LIKE ? ESCAPE '\\' \
                 OR sql LIKE ? ESCAPE '\\')",
            );
        }
        // The connection's own queries first, then the globals; alphabetical
        // within each group, case-insensitively so "Users" doesn't sort
        // before every lowercase name.
        query.push_str(" ORDER BY (locator = ?) ASC, name COLLATE NOCASE ASC");
        let mut prepared = sqlx::query(&query).bind(locator).bind(GLOBAL_SCOPE);
        if let Some(needle) = filtered {
            let pattern = format!("%{}%", escape_like(needle));
            prepared = prepared
                .bind(pattern.clone())
                .bind(pattern.clone())
                .bind(pattern);
        }
        let rows = prepared
            .bind(GLOBAL_SCOPE)
            .fetch_all(&self.pool)
            .await
            .map_err(|e| e.to_string())?;
        let mut queries = Vec::with_capacity(rows.len());
        for row in rows {
            queries.push(SavedQuery {
                id: row.try_get("id").map_err(|e| e.to_string())?,
                name: row.try_get("name").map_err(|e| e.to_string())?,
                description: row.try_get("description").map_err(|e| e.to_string())?,
                sql: row.try_get("sql").map_err(|e| e.to_string())?,
                locator: locator_of(row.try_get("locator").map_err(|e| e.to_string())?),
                updated_at: row.try_get("updated_at").map_err(|e| e.to_string())?,
            });
        }
        Ok(queries)
    }

    /// Deletes one saved query by id.
    pub async fn delete_saved_query(&self, id: i64) -> Result<(), String> {
        sqlx::query("DELETE FROM saved_queries WHERE id = ?")
            .bind(id)
            .execute(&self.pool)
            .await
            .map_err(|e| e.to_string())?;
        Ok(())
    }
}

/// Unix seconds now, or 0 if the clock is before the epoch.
fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
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

    #[test]
    fn an_absent_or_empty_locator_is_the_global_scope() {
        // The sentinel and its absence must decode to the same thing, or a
        // caller handing through an empty locator would write a scope no
        // connection can ever match and no list would show it again.
        assert_eq!(scope_of(None), GLOBAL_SCOPE);
        assert_eq!(scope_of(Some("")), GLOBAL_SCOPE);
        assert_eq!(scope_of(Some("/data/music.db")), "/data/music.db");
        assert_eq!(locator_of(GLOBAL_SCOPE.to_string()), None);
        assert_eq!(
            locator_of("/data/music.db".to_string()),
            Some("/data/music.db".to_string())
        );
    }
}
