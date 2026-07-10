use std::path::{Path, PathBuf};

use dioxus::prelude::*;

use crate::config::{default_config_path, ConnectionKind, SavedConnection, SavedList};
use crate::db::{ConnectionId, ConnectionRegistry, DbPool};

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
    /// Saved connections shown on the launch screen.
    pub saved: Signal<SavedList>,
    /// Which file each open tab came from, for "already open" detection.
    /// Keys are canonicalized where possible.
    pub open_paths: Signal<Vec<(ConnectionId, PathBuf)>>,
    /// Paths with a connect in flight, reserved before the pool open await.
    pub connecting: Signal<Vec<PathBuf>>,
    /// Error from the most recent connect/config operation, shown on the
    /// connections screen.
    pub connect_error: Signal<Option<String>>,
}

impl AppState {
    /// Must be called from a component (signals need the runtime).
    pub fn new() -> Self {
        // A failed config load surfaces on the launch screen and the app
        // starts with an empty list; SavedList remembers the failure and
        // refuses to persist over the unreadable file.
        let (saved, load_error) = match default_config_path() {
            Some(path) => {
                let (list, error) = SavedList::load(&path);
                (list, error.map(|e| e.to_string()))
            }
            None => (
                SavedList::load(Path::new("/nonexistent")).0,
                Some("no config directory found".to_string()),
            ),
        };
        Self {
            registry: Signal::new(ConnectionRegistry::default()),
            active: Signal::new(ActiveView::Connections),
            saved: Signal::new(saved),
            open_paths: Signal::new(Vec::new()),
            connecting: Signal::new(Vec::new()),
            connect_error: Signal::new(load_error),
        }
    }

    /// Adds a database file to the saved list (deduped by path) and
    /// persists the list.
    pub fn add_saved(mut self, path: PathBuf) {
        let path = canonical(&path);
        let added = self.saved.write().add(SavedConnection {
            name: tab_title(&path),
            kind: ConnectionKind::Sqlite,
            path,
        });
        if added {
            self.persist_saved();
        }
    }

    /// Removes a saved connection (open tabs are unaffected) and persists.
    pub fn remove_saved(mut self, path: &Path) {
        let removed = self.saved.write().remove(path);
        if removed {
            self.persist_saved();
        }
    }

    fn persist_saved(mut self) {
        let Some(config) = default_config_path() else {
            self.connect_error
                .set(Some("no config directory found".to_string()));
            return;
        };
        let result = self.saved.read().persist(&config);
        if let Err(err) = result {
            self.connect_error.set(Some(err.to_string()));
        }
    }

    /// Opens a saved connection in a new tab, or focuses the existing tab
    /// when the same file is already open. Pool creation happens before any
    /// signal is written, so no borrow spans the await; the `connecting`
    /// list reserves the path synchronously so a double-click can't open
    /// two tabs for the same file.
    pub async fn connect(mut self, path: PathBuf) {
        self.connect_error.set(None);
        let path = canonical(&path);
        let already_open = self
            .open_paths
            .read()
            .iter()
            .find(|(_, p)| *p == path)
            .map(|(id, _)| *id);
        if let Some(id) = already_open {
            self.active.set(ActiveView::Connection(id));
            return;
        }
        {
            let mut connecting = self.connecting.write();
            if connecting.contains(&path) {
                return;
            }
            connecting.push(path.clone());
        }
        let result = DbPool::open_sqlite(&path).await;
        self.connecting.write().retain(|p| p != &path);
        match result {
            Ok(pool) => {
                let id = self.registry.write().insert(tab_title(&path), pool);
                self.open_paths.write().push((id, path));
                self.active.set(ActiveView::Connection(id));
            }
            Err(err) => self.connect_error.set(Some(err.to_string())),
        }
    }

    /// Closes a tab: drops it from the registry, closes the pool in the
    /// background, and leaves the view somewhere sensible.
    pub fn close_connection(mut self, id: ConnectionId) {
        let removed = self.registry.write().remove(id);
        if let Some(connection) = removed {
            spawn(async move { connection.pool.close().await });
        }
        self.open_paths
            .write()
            .retain(|(open_id, _)| *open_id != id);
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

/// Canonicalizes for dedupe purposes; falls back to the given path when the
/// file is missing (the connect attempt will surface that error).
pub(crate) fn canonical(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
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

    #[test]
    fn canonical_falls_back_for_missing_files() {
        let missing = Path::new("/definitely/not/here.db");
        assert_eq!(canonical(missing), missing.to_path_buf());
    }
}
