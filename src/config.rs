//! Persistence for the saved-connections list (XDG config dir, TOML).

use std::fmt;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ConnectionKind {
    Sqlite,
}

/// One entry in the saved-connections list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedConnection {
    pub name: String,
    pub kind: ConnectionKind,
    /// Database file path (SQLite). Later kinds store their locator here or
    /// in kind-specific fields.
    pub path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigError(pub String);

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "config error: {}", self.0)
    }
}

impl std::error::Error for ConfigError {}

#[derive(Debug, Default, Serialize, Deserialize)]
struct ConnectionsFile {
    #[serde(default)]
    connections: Vec<SavedConnection>,
}

/// Default location: `$XDG_CONFIG_HOME/dataview/connections.toml`.
pub fn default_config_path() -> Option<PathBuf> {
    Some(
        dirs::config_dir()?
            .join("dataview")
            .join("connections.toml"),
    )
}

/// Loads the saved connections. A missing file is an empty list, not an
/// error; a malformed file is an error (don't silently drop user data).
pub fn load_connections(path: &Path) -> Result<Vec<SavedConnection>, ConfigError> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(ConfigError(format!("reading {}: {err}", path.display()))),
    };
    let file: ConnectionsFile =
        toml::from_str(&text).map_err(|err| ConfigError(format!("{}: {err}", path.display())))?;
    Ok(file.connections)
}

/// Saves the list, creating parent directories as needed. Writes to a temp
/// file and renames so a crash mid-write can't corrupt the config.
pub fn save_connections(path: &Path, connections: &[SavedConnection]) -> Result<(), ConfigError> {
    let file = ConnectionsFile {
        connections: connections.to_vec(),
    };
    let text = toml::to_string_pretty(&file).map_err(|err| ConfigError(err.to_string()))?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|err| ConfigError(format!("creating {}: {err}", parent.display())))?;
    }
    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, text)
        .map_err(|err| ConfigError(format!("writing {}: {err}", tmp.display())))?;
    std::fs::rename(&tmp, path)
        .map_err(|err| ConfigError(format!("replacing {}: {err}", path.display())))
}

/// The saved-connections list plus the load outcome. Mutations go through
/// this type so a list that failed to load is never persisted back over the
/// user's file.
#[derive(Debug, Clone, PartialEq)]
pub struct SavedList {
    entries: Vec<SavedConnection>,
    load_failed: bool,
}

impl SavedList {
    pub fn load(path: &Path) -> (Self, Option<ConfigError>) {
        match load_connections(path) {
            Ok(entries) => (
                Self {
                    entries,
                    load_failed: false,
                },
                None,
            ),
            Err(err) => (
                Self {
                    entries: Vec::new(),
                    load_failed: true,
                },
                Some(err),
            ),
        }
    }

    pub fn entries(&self) -> &[SavedConnection] {
        &self.entries
    }

    /// Adds unless an entry with the same path exists. Returns whether the
    /// list changed.
    pub fn add(&mut self, connection: SavedConnection) -> bool {
        if self.entries.iter().any(|s| s.path == connection.path) {
            return false;
        }
        self.entries.push(connection);
        true
    }

    /// Removes the entry with this path. Returns whether the list changed.
    pub fn remove(&mut self, path: &Path) -> bool {
        let before = self.entries.len();
        self.entries.retain(|s| s.path != path);
        self.entries.len() != before
    }

    /// Persists the list — unless it came from an unreadable file, in which
    /// case refusing protects whatever the user had saved there.
    pub fn persist(&self, path: &Path) -> Result<(), ConfigError> {
        if self.load_failed {
            return Err(ConfigError(format!(
                "not overwriting {} because it could not be read at startup; fix or delete it and restart",
                path.display()
            )));
        }
        save_connections(path, &self.entries)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn saved(name: &str, path: &str) -> SavedConnection {
        SavedConnection {
            name: name.into(),
            kind: ConnectionKind::Sqlite,
            path: PathBuf::from(path),
        }
    }

    #[test]
    fn missing_file_loads_as_empty_list() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nope").join("connections.toml");
        assert_eq!(load_connections(&path).unwrap(), Vec::new());
    }

    #[test]
    fn save_and_load_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("deep").join("connections.toml");
        let connections = vec![
            saved("music.db", "/data/music.db"),
            saved("with späce.db", "/tmp/with späce.db"),
        ];
        save_connections(&path, &connections).unwrap();
        assert_eq!(load_connections(&path).unwrap(), connections);
    }

    #[test]
    fn malformed_file_is_an_error_not_data_loss() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("connections.toml");
        std::fs::write(&path, "connections = \"not a list\"").unwrap();
        assert!(load_connections(&path).is_err());
    }

    #[test]
    fn empty_file_loads_as_empty_list() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("connections.toml");
        std::fs::write(&path, "").unwrap();
        assert_eq!(load_connections(&path).unwrap(), Vec::new());
    }

    #[test]
    fn failed_load_refuses_to_persist_over_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("connections.toml");
        let garbage = "connections = \"not a list\"";
        std::fs::write(&path, garbage).unwrap();

        let (mut list, error) = SavedList::load(&path);
        assert!(error.is_some());
        assert!(list.entries().is_empty());

        assert!(list.add(saved("a.db", "/tmp/a.db")));
        assert!(list.persist(&path).is_err());
        // The unreadable file is untouched, not replaced by the empty list.
        assert_eq!(std::fs::read_to_string(&path).unwrap(), garbage);
    }

    #[test]
    fn add_dedupes_by_path_and_remove_reports_changes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("connections.toml");
        let (mut list, error) = SavedList::load(&path);
        assert!(error.is_none());

        assert!(list.add(saved("a.db", "/tmp/a.db")));
        assert!(!list.add(saved("other name", "/tmp/a.db")));
        assert_eq!(list.entries().len(), 1);

        list.persist(&path).unwrap();
        let (reloaded, _) = SavedList::load(&path);
        assert_eq!(reloaded.entries(), list.entries());

        assert!(!list.remove(Path::new("/tmp/missing.db")));
        assert!(list.remove(Path::new("/tmp/a.db")));
        assert!(list.entries().is_empty());
    }
}
