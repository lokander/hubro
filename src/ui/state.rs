use std::path::{Path, PathBuf};

use dioxus::prelude::*;

use crate::db::{ConnectionId, ConnectionRegistry, DbError, DbPool};

/// Which screen the main panel shows: the connections screen or one open
/// connection tab.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActiveView {
    Connections,
    Connection(ConnectionId),
}

/// App-wide state provided via context. `Copy` because it only holds signals.
#[derive(Clone, Copy)]
pub struct AppState {
    pub registry: Signal<ConnectionRegistry>,
    pub active: Signal<ActiveView>,
    /// Error from the most recent connection attempt, shown on the
    /// connections screen.
    pub connect_error: Signal<Option<DbError>>,
}

impl AppState {
    /// Must be called from a component (signals need the runtime).
    pub fn new() -> Self {
        Self {
            registry: Signal::new(ConnectionRegistry::default()),
            active: Signal::new(ActiveView::Connections),
            connect_error: Signal::new(None),
        }
    }

    /// Opens a SQLite file, registers it as a new tab, and switches to it.
    /// Pool creation happens before any signal is written, so no borrow
    /// spans the await.
    pub async fn open_sqlite(mut self, path: PathBuf) {
        match DbPool::open_sqlite(&path).await {
            Ok(pool) => {
                let id = self.registry.write().insert(tab_title(&path), pool);
                self.connect_error.set(None);
                self.active.set(ActiveView::Connection(id));
            }
            Err(err) => self.connect_error.set(Some(err)),
        }
    }

    /// Closes a tab: drops it from the registry, closes the pool in the
    /// background, and leaves the view somewhere sensible.
    pub fn close_connection(mut self, id: ConnectionId) {
        let removed = self.registry.write().remove(id);
        if let Some(connection) = removed {
            spawn(async move { connection.pool.close().await });
        }
        if *self.active.read() == ActiveView::Connection(id) {
            self.active.set(ActiveView::Connections);
        }
    }
}

impl Default for AppState {
    fn default() -> Self {
        Self::new()
    }
}

/// Tab label for a database file: the file name, or the whole path when
/// there is no file name component.
pub fn tab_title(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tab_title_uses_the_file_name() {
        assert_eq!(tab_title(Path::new("/data/apps/music.db")), "music.db");
        assert_eq!(tab_title(Path::new("plain.sqlite")), "plain.sqlite");
    }

    #[test]
    fn tab_title_falls_back_to_the_full_path() {
        assert_eq!(tab_title(Path::new("/")), "/");
    }
}
