use std::path::Path;

use sqlx::sqlite::SqlitePool;

use super::error::DbError;
use super::schema::TableMeta;
use super::sqlite;
use super::value::QueryResult;

/// Stable handle for one open connection (one tab in the UI).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ConnectionId(u64);

/// A pool for one open database. Cheap to clone (drivers use `Arc`
/// internally), so async tasks can grab a copy instead of borrowing state
/// across an await point.
#[derive(Clone)]
pub enum DbPool {
    Sqlite(SqlitePool),
}

impl DbPool {
    /// Opens an existing SQLite database file.
    pub async fn open_sqlite(path: &Path) -> Result<DbPool, DbError> {
        Ok(DbPool::Sqlite(sqlite::open_sqlite(path).await?))
    }

    pub async fn query(&self, sql: &str) -> Result<QueryResult, DbError> {
        match self {
            DbPool::Sqlite(pool) => sqlite::query(pool, sql).await,
        }
    }

    pub async fn introspect(&self) -> Result<Vec<TableMeta>, DbError> {
        match self {
            DbPool::Sqlite(pool) => sqlite::introspect(pool).await,
        }
    }

    pub async fn close(&self) {
        match self {
            DbPool::Sqlite(pool) => pool.close().await,
        }
    }
}

/// One open connection: a display name plus its pool.
#[derive(Clone)]
pub struct Connection {
    pub id: ConnectionId,
    pub name: String,
    pub pool: DbPool,
}

/// All simultaneously open connections, in tab order.
///
/// Sync by design: open pools first (await), then insert. The registry lives
/// in a signal, and inserting through `.write()` must not span an await.
#[derive(Default)]
pub struct ConnectionRegistry {
    next_id: u64,
    connections: Vec<Connection>,
}

impl ConnectionRegistry {
    pub fn insert(&mut self, name: impl Into<String>, pool: DbPool) -> ConnectionId {
        let id = ConnectionId(self.next_id);
        self.next_id += 1;
        self.connections.push(Connection {
            id,
            name: name.into(),
            pool,
        });
        id
    }

    pub fn get(&self, id: ConnectionId) -> Option<&Connection> {
        self.connections.iter().find(|c| c.id == id)
    }

    /// Removes and returns the connection; callers should `close()` its pool
    /// from an async task.
    pub fn remove(&mut self, id: ConnectionId) -> Option<Connection> {
        let idx = self.connections.iter().position(|c| c.id == id)?;
        Some(self.connections.remove(idx))
    }

    pub fn iter(&self) -> impl Iterator<Item = &Connection> {
        self.connections.iter()
    }

    pub fn len(&self) -> usize {
        self.connections.len()
    }

    pub fn is_empty(&self) -> bool {
        self.connections.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dummy_pool() -> DbPool {
        // A lazily-connecting pool is fine for registry bookkeeping tests,
        // but sqlx still wants a Tokio context to construct it.
        DbPool::Sqlite(SqlitePool::connect_lazy("sqlite::memory:").unwrap())
    }

    #[tokio::test]
    async fn insert_assigns_unique_ids_in_tab_order() {
        let mut registry = ConnectionRegistry::default();
        let a = registry.insert("a.db", dummy_pool());
        let b = registry.insert("b.db", dummy_pool());
        assert_ne!(a, b);
        let names: Vec<&str> = registry.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["a.db", "b.db"]);
    }

    #[tokio::test]
    async fn remove_frees_the_entry_but_never_reuses_ids() {
        let mut registry = ConnectionRegistry::default();
        let a = registry.insert("a.db", dummy_pool());
        assert!(registry.remove(a).is_some());
        assert!(registry.remove(a).is_none());
        assert!(registry.is_empty());
        let b = registry.insert("b.db", dummy_pool());
        assert_ne!(a, b);
        assert_eq!(registry.len(), 1);
        assert_eq!(registry.get(b).unwrap().name, "b.db");
    }
}
