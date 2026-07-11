//! Persistence for the saved-connections list (XDG config dir, TOML).

use std::fmt;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::tunnel::TunnelConfig;

/// One entry in the saved-connections list. Internally tagged on `kind`, so
/// existing `kind = "sqlite"` + `path` TOML entries keep deserializing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum SavedConnection {
    Sqlite {
        name: String,
        path: PathBuf,
    },
    Postgres {
        name: String,
        /// Connection URL **without** a password — credentials never live in
        /// the config file (keyring persistence arrives with FRE-27).
        url: String,
        /// Optional SSH tunnel to reach the server through. `default` +
        /// `skip_serializing_if` keep pre-tunnel config files (and files
        /// written for tunnel-less connections) unchanged.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tunnel: Option<TunnelConfig>,
    },
}

impl SavedConnection {
    pub fn name(&self) -> &str {
        match self {
            SavedConnection::Sqlite { name, .. } | SavedConnection::Postgres { name, .. } => name,
        }
    }

    /// Stable identity used for dedupe and display: the file path or the
    /// stored URL.
    pub fn locator(&self) -> String {
        match self {
            SavedConnection::Sqlite { path, .. } => path.display().to_string(),
            SavedConnection::Postgres { url, .. } => url.clone(),
        }
    }
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

/// Which theme the app uses. `System` follows the OS preference; `Light`
/// and `Dark` are manual overrides. Serialized lowercase in settings.toml.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Theme {
    #[default]
    System,
    Light,
    Dark,
}

impl Theme {
    /// Resolves to a concrete dark/light choice: `System` defers to the OS
    /// preference, explicit choices ignore it.
    pub fn resolve_dark(self, system_prefers_dark: bool) -> bool {
        match self {
            Theme::System => system_prefers_dark,
            Theme::Light => false,
            Theme::Dark => true,
        }
    }

    /// Cycles System → Light → Dark → System for the toggle control.
    pub fn next(self) -> Theme {
        match self {
            Theme::System => Theme::Light,
            Theme::Light => Theme::Dark,
            Theme::Dark => Theme::System,
        }
    }

    /// Short label for the toggle control.
    pub fn label(self) -> &'static str {
        match self {
            Theme::System => "System",
            Theme::Light => "Light",
            Theme::Dark => "Dark",
        }
    }
}

/// User preferences, persisted separately from the connections list so a
/// corrupt settings file never blocks connecting to databases.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Settings {
    #[serde(default)]
    pub theme: Theme,
}

/// Default location: `$XDG_CONFIG_HOME/dataview/settings.toml`.
pub fn default_settings_path() -> Option<PathBuf> {
    Some(dirs::config_dir()?.join("dataview").join("settings.toml"))
}

/// Loads settings. A missing *or* malformed file yields defaults — these are
/// non-critical UI preferences, so (unlike the connections list) a bad file
/// never surfaces an error or blocks the app; the user just gets defaults.
pub fn load_settings(path: &Path) -> Settings {
    match std::fs::read_to_string(path) {
        Ok(text) => toml::from_str(&text).unwrap_or_default(),
        Err(_) => Settings::default(),
    }
}

/// Persists settings, creating parent dirs and writing via a temp file +
/// rename so a crash mid-write can't corrupt the file.
pub fn save_settings(path: &Path, settings: &Settings) -> Result<(), ConfigError> {
    let text = toml::to_string_pretty(settings).map_err(|err| ConfigError(err.to_string()))?;
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

    /// Adds unless an entry with the same locator exists. Returns whether
    /// the list changed.
    ///
    /// Re-adding an existing Postgres URL keeps the entry (and its name) but
    /// adopts a changed tunnel config, so reconnecting with different tunnel
    /// settings persists them.
    pub fn add(&mut self, connection: SavedConnection) -> bool {
        let existing = self
            .entries
            .iter_mut()
            .find(|s| s.locator() == connection.locator());
        let Some(existing) = existing else {
            self.entries.push(connection);
            return true;
        };
        if let (
            SavedConnection::Postgres { tunnel, .. },
            SavedConnection::Postgres {
                tunnel: new_tunnel, ..
            },
        ) = (existing, &connection)
        {
            if *tunnel != *new_tunnel {
                *tunnel = new_tunnel.clone();
                return true;
            }
        }
        false
    }

    /// Removes and returns the entry with this locator (`None` when absent).
    pub fn remove(&mut self, locator: &str) -> Option<SavedConnection> {
        let index = self.entries.iter().position(|s| s.locator() == locator)?;
        Some(self.entries.remove(index))
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
        SavedConnection::Sqlite {
            name: name.into(),
            path: PathBuf::from(path),
        }
    }

    fn saved_pg(name: &str, url: &str) -> SavedConnection {
        SavedConnection::Postgres {
            name: name.into(),
            url: url.into(),
            tunnel: None,
        }
    }

    fn tunnel(auth: crate::tunnel::TunnelAuth) -> TunnelConfig {
        TunnelConfig {
            host: "bastion.example.com".into(),
            port: 2222,
            user: "deploy".into(),
            auth,
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
            saved_pg(
                "prod",
                "postgres://user@db.example.com:5432/app?sslmode=require",
            ),
        ];
        save_connections(&path, &connections).unwrap();
        assert_eq!(load_connections(&path).unwrap(), connections);
    }

    #[test]
    fn tunnel_config_round_trips_and_serializes_tagged() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("connections.toml");
        let connections = vec![
            SavedConnection::Postgres {
                name: "via agent".into(),
                url: "postgres://u@db.internal:5432/app".into(),
                tunnel: Some(tunnel(crate::tunnel::TunnelAuth::Agent)),
            },
            SavedConnection::Postgres {
                name: "via key".into(),
                url: "postgres://u@db2.internal:5432/app".into(),
                tunnel: Some(tunnel(crate::tunnel::TunnelAuth::KeyFile {
                    path: PathBuf::from("/home/u/.ssh/id_ed25519"),
                })),
            },
        ];
        save_connections(&path, &connections).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        // TOML-friendly tagged forms, no secrets.
        assert!(text.contains("method = \"agent\""));
        assert!(text.contains("method = \"keyfile\""));
        assert!(!text.to_lowercase().contains("passphrase"));
        assert_eq!(load_connections(&path).unwrap(), connections);
    }

    #[test]
    fn tunnel_less_postgres_entries_serialize_without_a_tunnel_key() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("connections.toml");
        save_connections(&path, &[saved_pg("prod", "postgres://u@h:5432/db")]).unwrap();
        assert!(!std::fs::read_to_string(&path).unwrap().contains("tunnel"));
    }

    #[test]
    fn legacy_postgres_entries_without_tunnel_still_deserialize() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("connections.toml");
        // Format written before the tunnel field existed.
        std::fs::write(
            &path,
            "[[connections]]\nname = \"prod\"\nkind = \"postgres\"\nurl = \"postgres://u@h:5432/db\"\n",
        )
        .unwrap();
        assert_eq!(
            load_connections(&path).unwrap(),
            vec![saved_pg("prod", "postgres://u@h:5432/db")]
        );
    }

    #[test]
    fn add_updates_the_tunnel_of_an_existing_entry() {
        let dir = tempfile::tempdir().unwrap();
        let (mut list, _) = SavedList::load(&dir.path().join("connections.toml"));
        assert!(list.add(saved_pg("prod", "postgres://u@h:5432/db")));
        // Same URL, tunnel added: entry is updated (and keeps its name).
        let with_tunnel = SavedConnection::Postgres {
            name: "ignored".into(),
            url: "postgres://u@h:5432/db".into(),
            tunnel: Some(tunnel(crate::tunnel::TunnelAuth::Agent)),
        };
        assert!(list.add(with_tunnel.clone()));
        assert_eq!(list.entries().len(), 1);
        match &list.entries()[0] {
            SavedConnection::Postgres { name, tunnel, .. } => {
                assert_eq!(name, "prod");
                assert_eq!(tunnel.as_ref().unwrap().host, "bastion.example.com");
            }
            other => panic!("unexpected entry {other:?}"),
        }
        // Identical tunnel again: no change.
        assert!(!list.add(with_tunnel));
    }

    #[test]
    fn legacy_sqlite_entries_still_deserialize() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("connections.toml");
        // Format written before the Postgres variant existed.
        std::fs::write(
            &path,
            "[[connections]]\nname = \"music.db\"\nkind = \"sqlite\"\npath = \"/data/music.db\"\n",
        )
        .unwrap();
        assert_eq!(
            load_connections(&path).unwrap(),
            vec![saved("music.db", "/data/music.db")]
        );
    }

    #[test]
    fn add_dedupes_postgres_by_url() {
        let dir = tempfile::tempdir().unwrap();
        let (mut list, _) = SavedList::load(&dir.path().join("connections.toml"));
        assert!(list.add(saved_pg("prod", "postgres://u@h:5432/db")));
        assert!(!list.add(saved_pg("other name", "postgres://u@h:5432/db")));
        assert!(list.remove("postgres://u@h:5432/db").is_some());
        assert!(list.entries().is_empty());
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
    fn theme_serde_round_trips_lowercase() {
        for (theme, token) in [
            (Theme::System, "\"system\""),
            (Theme::Light, "\"light\""),
            (Theme::Dark, "\"dark\""),
        ] {
            assert_eq!(toml::Value::try_from(theme).unwrap().to_string(), token);
            let settings = Settings { theme };
            let text = toml::to_string(&settings).unwrap();
            assert_eq!(toml::from_str::<Settings>(&text).unwrap(), settings);
        }
    }

    #[test]
    fn theme_resolves_dark_from_system_preference() {
        assert!(Theme::System.resolve_dark(true));
        assert!(!Theme::System.resolve_dark(false));
        // Explicit choices ignore the system preference.
        assert!(!Theme::Light.resolve_dark(true));
        assert!(Theme::Dark.resolve_dark(false));
    }

    #[test]
    fn theme_next_cycles_system_light_dark() {
        assert_eq!(Theme::System.next(), Theme::Light);
        assert_eq!(Theme::Light.next(), Theme::Dark);
        assert_eq!(Theme::Dark.next(), Theme::System);
    }

    #[test]
    fn missing_settings_file_loads_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nope").join("settings.toml");
        assert_eq!(load_settings(&path).theme, Theme::System);
    }

    #[test]
    fn malformed_settings_file_falls_back_to_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.toml");
        std::fs::write(&path, "theme = 42").unwrap();
        assert_eq!(load_settings(&path).theme, Theme::System);
    }

    #[test]
    fn settings_save_and_load_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("deep").join("settings.toml");
        let settings = Settings { theme: Theme::Dark };
        save_settings(&path, &settings).unwrap();
        assert_eq!(load_settings(&path), settings);
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

        assert!(list.remove("/tmp/missing.db").is_none());
        assert!(list.remove("/tmp/a.db").is_some());
        assert!(list.entries().is_empty());
    }
}
