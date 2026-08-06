//! Persistence for the saved-connections list (XDG config dir, TOML).

use std::fmt;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::azure::EntraAuth;
use crate::db::Dialect;
use crate::tunnel::TunnelConfig;

/// How a server connection (Postgres, FRE-43; SQL Server, FRE-58)
/// authenticates. `Password` (the default) resolves a password from session
/// memory / the keyring / a prompt; `Entra` acquires a Microsoft Entra ID
/// access token and uses it in place of the password. (Named for the backend
/// it landed on first; the shape is backend-neutral.)
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum PgAuth {
    #[default]
    Password,
    Entra(EntraAuth),
}

impl PgAuth {
    /// Whether this is the default password mode — lets the config skip writing
    /// an `[…auth]` key for ordinary connections (back-compat).
    pub fn is_password(&self) -> bool {
        matches!(self, PgAuth::Password)
    }
}

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
        /// the config file; passwords are stored in the OS keyring. Canonical
        /// form (FRE-39), so it doubles as the keyring account key.
        url: String,
        /// Optional SSH tunnel to reach the server through. `default` +
        /// `skip_serializing_if` keep pre-tunnel config files (and files
        /// written for tunnel-less connections) unchanged.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tunnel: Option<TunnelConfig>,
        /// Authentication mode (FRE-43). `default` + `skip_serializing_if` keep
        /// ordinary password connections' TOML unchanged, and pre-Entra config
        /// files (no `auth` key) deserialize as `Password`.
        #[serde(default, skip_serializing_if = "PgAuth::is_password")]
        auth: PgAuth,
    },
    /// SQL Server (FRE-57), serialized with `kind = "sqlserver"`. Like
    /// Postgres, the URL is stored **without** a password in the canonical
    /// form (see [`crate::db::normalize_mssql_url`]) and doubles as the
    /// keyring account key.
    SqlServer {
        name: String,
        url: String,
        /// Optional SSH tunnel (FRE-58), stored exactly like the Postgres
        /// one. `default` + `skip_serializing_if` keep FRE-57-era files (and
        /// tunnel-less entries) deserializing and serializing unchanged.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tunnel: Option<TunnelConfig>,
        /// Authentication mode (FRE-58), stored exactly like the Postgres
        /// one; a missing `auth` key deserializes as `Password`.
        #[serde(default, skip_serializing_if = "PgAuth::is_password")]
        auth: PgAuth,
    },
}

/// Which database backend a connection targets. Purely in-memory — the config
/// file keys `SavedConnection` on its serde `kind` tag instead — so adding a
/// variant here never touches the on-disk format.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackendKind {
    Sqlite,
    Postgres,
    SqlServer,
}

impl From<BackendKind> for Dialect {
    /// A saved connection's backend and an open connection's SQL dialect name
    /// the same three engines, so converting lets both address one per-engine
    /// lookup (e.g. the brand marks, FRE-70).
    fn from(kind: BackendKind) -> Self {
        match kind {
            BackendKind::Sqlite => Dialect::Sqlite,
            BackendKind::Postgres => Dialect::Postgres,
            BackendKind::SqlServer => Dialect::SqlServer,
        }
    }
}

impl SavedConnection {
    pub fn name(&self) -> &str {
        match self {
            SavedConnection::Sqlite { name, .. }
            | SavedConnection::Postgres { name, .. }
            | SavedConnection::SqlServer { name, .. } => name,
        }
    }

    pub fn backend(&self) -> BackendKind {
        match self {
            SavedConnection::Sqlite { .. } => BackendKind::Sqlite,
            SavedConnection::Postgres { .. } => BackendKind::Postgres,
            SavedConnection::SqlServer { .. } => BackendKind::SqlServer,
        }
    }

    /// Stable identity used for dedupe and display: the file path or the
    /// stored URL.
    pub fn locator(&self) -> String {
        match self {
            SavedConnection::Sqlite { path, .. } => path.display().to_string(),
            SavedConnection::Postgres { url, .. } | SavedConnection::SqlServer { url, .. } => {
                url.clone()
            }
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

/// Sensible bounds so a corrupt (or hand-edited) geometry can never produce
/// an unusable window: a sub-minimum or non-finite size falls back to the
/// launch default, and a wildly out-of-range position is dropped.
pub const MIN_WINDOW_WIDTH: f64 = 480.0;
pub const MIN_WINDOW_HEIGHT: f64 = 360.0;
pub const MAX_WINDOW_DIM: f64 = 16_384.0;
/// Launch size used when no geometry is saved (the historical hard-coded
/// WindowBuilder size).
pub const DEFAULT_WINDOW_WIDTH: f64 = 1200.0;
pub const DEFAULT_WINDOW_HEIGHT: f64 = 800.0;

/// Persisted window size/position, in logical (scale-factor-independent)
/// pixels so a display move between monitors of different DPI restores sanely.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct WindowGeometry {
    pub width: f64,
    pub height: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub x: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub y: Option<f64>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub maximized: bool,
}

impl Default for WindowGeometry {
    fn default() -> Self {
        WindowGeometry {
            width: DEFAULT_WINDOW_WIDTH,
            height: DEFAULT_WINDOW_HEIGHT,
            x: None,
            y: None,
            maximized: false,
        }
    }
}

impl WindowGeometry {
    /// Clamps to sane bounds, always yielding a usable geometry: a
    /// sub-minimum, huge, or non-finite size falls back into
    /// `[MIN, MAX]` (a corrupt tiny/negative size can't make an unusable
    /// window), and a non-finite or wildly out-of-range position is dropped
    /// so the OS/WM places the window instead.
    pub fn sanitized(self) -> Self {
        let width = if self.width.is_finite() {
            self.width.clamp(MIN_WINDOW_WIDTH, MAX_WINDOW_DIM)
        } else {
            DEFAULT_WINDOW_WIDTH
        };
        let height = if self.height.is_finite() {
            self.height.clamp(MIN_WINDOW_HEIGHT, MAX_WINDOW_DIM)
        } else {
            DEFAULT_WINDOW_HEIGHT
        };
        let clean = |v: Option<f64>| v.filter(|p| p.is_finite() && p.abs() <= MAX_WINDOW_DIM);
        WindowGeometry {
            width,
            height,
            x: clean(self.x),
            y: clean(self.y),
            maximized: self.maximized,
        }
    }
}

/// User preferences, persisted separately from the connections list so a
/// corrupt settings file never blocks connecting to databases.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Settings {
    #[serde(default)]
    pub theme: Theme,
    /// Last window size/position (FRE-30). `None` until the window is first
    /// resized/moved; on launch a missing value means "use the default size".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window: Option<WindowGeometry>,
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

/// Persists just the theme, preserving the rest of the settings file (window
/// geometry). Loads the current file first so a concurrent field isn't lost —
/// theme and window geometry are written from different code paths.
pub fn save_theme(path: &Path, theme: Theme) -> Result<(), ConfigError> {
    let mut settings = load_settings(path);
    settings.theme = theme;
    save_settings(path, &settings)
}

/// Persists just the window geometry, preserving the theme (see
/// [`save_theme`] for why the file is re-read first).
pub fn save_window_geometry(path: &Path, geometry: WindowGeometry) -> Result<(), ConfigError> {
    let mut settings = load_settings(path);
    settings.window = Some(geometry);
    save_settings(path, &settings)
}

/// Which pane a restored tab shows. Mirrors `ui::state::Pane`, but kept here
/// so the config layer never depends on the UI; serialized lowercase.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SessionPane {
    #[default]
    Browser,
    Sql,
    /// The schema pane (FRE-69).
    Schema,
}

/// Deserializes a [`SessionPane`], treating anything this build doesn't
/// recognize as the default rather than failing the whole session.
fn pane_or_default<'de, D>(deserializer: D) -> Result<SessionPane, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw = String::deserialize(deserializer)?;
    Ok(match raw.as_str() {
        "sql" => SessionPane::Sql,
        "schema" => SessionPane::Schema,
        _ => SessionPane::Browser,
    })
}

/// One open connection tab, remembered for the next launch.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct SessionTab {
    /// Open-locator (canonical SQLite path or Postgres URL) — matched against
    /// the saved-connections list at restore time.
    pub locator: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selected_schema: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selected_table: Option<String>,
    /// Tolerant of values this build doesn't know: serde treats an unknown
    /// enum variant as a hard error, and [`load_session`] discards the whole
    /// session on any parse failure — so a build that predates a new pane
    /// would silently drop every restored tab, not just the pane. Unknown
    /// values fall back to the default instead (FRE-69).
    #[serde(default, deserialize_with = "pane_or_default")]
    pub pane: SessionPane,
}

/// The last session (FRE-30): open tabs in order, plus which one was active.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Session {
    #[serde(default)]
    pub tabs: Vec<SessionTab>,
    /// Locator of the active tab, or `None` for the connections screen.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active: Option<String>,
}

/// Default location: `$XDG_CONFIG_HOME/dataview/session.toml`. Kept separate
/// from `settings.toml` because it is transient, churns often, and is fine to
/// lose — whereas a corrupt settings file must not take user preferences with
/// it.
pub fn default_session_path() -> Option<PathBuf> {
    Some(dirs::config_dir()?.join("dataview").join("session.toml"))
}

/// Loads the last session. A missing *or* malformed file yields an empty
/// session (never an error): restore is best-effort and must never block or
/// crash startup.
pub fn load_session(path: &Path) -> Session {
    match std::fs::read_to_string(path) {
        Ok(text) => toml::from_str(&text).unwrap_or_default(),
        Err(_) => Session::default(),
    }
}

/// Persists the session, creating parent dirs and writing via a temp file +
/// rename so a crash mid-write can't corrupt it.
pub fn save_session(path: &Path, session: &Session) -> Result<(), ConfigError> {
    let text = toml::to_string_pretty(session).map_err(|err| ConfigError(err.to_string()))?;
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

/// A saved connection reduced to what session-restore planning needs: its
/// open-locator (canonical) form and which backend it targets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RestoreCandidate {
    pub locator: String,
    pub backend: BackendKind,
}

/// Decides which remembered tabs to auto-reopen (pure, so it is unit-testable
/// without a database or keyring).
///
/// A tab is reopened only when its locator still matches a saved connection —
/// ad-hoc connections the user never saved are not resurrected. SQLite
/// reconnects unconditionally; server backends (Postgres, SQL Server)
/// reconnect only when a password is available (`password_available`: session
/// memory or keyring), so startup never pops a wall of password prompts.
/// Session order (and any duplicates) is preserved.
pub fn plan_session_restore(
    tabs: &[SessionTab],
    candidates: &[RestoreCandidate],
    password_available: impl Fn(&str) -> bool,
) -> Vec<SessionTab> {
    tabs.iter()
        .filter(
            |tab| match candidates.iter().find(|c| c.locator == tab.locator) {
                None => false,
                Some(c) => match c.backend {
                    BackendKind::Postgres | BackendKind::SqlServer => {
                        password_available(&tab.locator)
                    }
                    BackendKind::Sqlite => true,
                },
            },
        )
        .cloned()
        .collect()
}

/// The saved-connections list plus the load outcome. Mutations go through
/// this type so a list that failed to load is never persisted back over the
/// user's file.
#[derive(Debug, Clone, PartialEq)]
pub struct SavedList {
    entries: Vec<SavedConnection>,
    load_failed: bool,
}

/// Canonicalizes Postgres (FRE-39) and SQL Server URLs and drops entries that
/// collapse to an already-present locator — upgrading a list saved before
/// normalization existed (e.g. `postgresql://` or a portless URL) and
/// de-duplicating it. The first entry for each locator wins, so its name is
/// kept.
fn normalize_and_dedup(entries: Vec<SavedConnection>) -> Vec<SavedConnection> {
    let mut out: Vec<SavedConnection> = Vec::new();
    for mut entry in entries {
        match &mut entry {
            SavedConnection::Postgres { url, .. } => {
                if let Ok(normalized) = crate::db::normalize_pg_url(url) {
                    *url = normalized;
                }
            }
            SavedConnection::SqlServer { url, .. } => {
                if let Ok(normalized) = crate::db::normalize_mssql_url(url) {
                    *url = normalized;
                }
            }
            SavedConnection::Sqlite { .. } => {}
        }
        let locator = entry.locator();
        if !out.iter().any(|existing| existing.locator() == locator) {
            out.push(entry);
        }
    }
    out
}

impl SavedList {
    pub fn load(path: &Path) -> (Self, Option<ConfigError>) {
        match load_connections(path) {
            Ok(entries) => (
                Self {
                    entries: normalize_and_dedup(entries),
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
    /// Re-adding an existing Postgres or SQL Server URL keeps the entry (and
    /// its name) but adopts a changed tunnel config or auth mode, so
    /// reconnecting with different tunnel/auth settings persists them.
    pub fn add(&mut self, connection: SavedConnection) -> bool {
        let existing = self
            .entries
            .iter_mut()
            .find(|s| s.locator() == connection.locator());
        let Some(existing) = existing else {
            self.entries.push(connection);
            return true;
        };
        match (existing, &connection) {
            (
                SavedConnection::Postgres { tunnel, auth, .. },
                SavedConnection::Postgres {
                    tunnel: new_tunnel,
                    auth: new_auth,
                    ..
                },
            )
            | (
                SavedConnection::SqlServer { tunnel, auth, .. },
                SavedConnection::SqlServer {
                    tunnel: new_tunnel,
                    auth: new_auth,
                    ..
                },
            ) => {
                let mut changed = false;
                if *tunnel != *new_tunnel {
                    *tunnel = new_tunnel.clone();
                    changed = true;
                }
                if *auth != *new_auth {
                    *auth = new_auth.clone();
                    changed = true;
                }
                changed
            }
            _ => false,
        }
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
            auth: PgAuth::Password,
        }
    }

    fn saved_ms(name: &str, url: &str) -> SavedConnection {
        SavedConnection::SqlServer {
            name: name.into(),
            url: url.into(),
            tunnel: None,
            auth: PgAuth::Password,
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
    fn load_normalizes_and_dedups_equivalent_postgres_entries() {
        // A list saved before normalization: three spellings of one server plus
        // a distinct one.
        let entries = vec![
            saved_pg("primary", "postgresql://u@Host/db"),
            saved_pg("dup-a", "postgres://u@host:5432/db"),
            saved_pg("dup-b", "postgres://u@host/db"),
            saved_pg("other", "postgres://u@other:5432/db2"),
        ];
        let deduped = normalize_and_dedup(entries);

        assert_eq!(
            deduped.len(),
            2,
            "the three equivalent forms collapse to one"
        );
        // First entry wins (its name is kept) and its URL is canonical.
        assert_eq!(deduped[0].name(), "primary");
        assert_eq!(deduped[0].locator(), "postgres://u@host:5432/db");
        assert_eq!(deduped[1].name(), "other");
        // Sqlite entries pass through untouched.
        let mixed = normalize_and_dedup(vec![saved("m.db", "/data/m.db")]);
        assert_eq!(mixed.len(), 1);
    }

    #[test]
    fn sqlserver_entries_round_trip_with_the_sqlserver_kind_tag() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("connections.toml");
        let entries = vec![saved_ms(
            "mssql prod",
            "mssql://sa@db.example.com:1433/app?encrypt=on",
        )];
        save_connections(&path, &entries).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("kind = \"sqlserver\""));
        assert!(!text.contains("password"));
        assert_eq!(load_connections(&path).unwrap(), entries);
        // A hand-written minimal entry (the on-disk shape) deserializes too.
        std::fs::write(
            &path,
            "[[connections]]\nkind = \"sqlserver\"\nname = \"ms\"\nurl = \"mssql://sa@h:1433/db\"\n",
        )
        .unwrap();
        assert_eq!(
            load_connections(&path).unwrap(),
            vec![saved_ms("ms", "mssql://sa@h:1433/db")]
        );
    }

    #[test]
    fn load_normalizes_and_dedups_equivalent_sqlserver_entries() {
        // Three spellings of one server (sqlserver:// scheme, portless, cased
        // host) plus a distinct one.
        let entries = vec![
            saved_ms("primary", "sqlserver://sa@Host/db"),
            saved_ms("dup-a", "mssql://sa@host:1433/db"),
            saved_ms("dup-b", "mssql://sa@host/db"),
            saved_ms("other", "mssql://sa@other:1433/db2"),
        ];
        let deduped = normalize_and_dedup(entries);
        assert_eq!(deduped.len(), 2);
        assert_eq!(deduped[0].name(), "primary");
        assert_eq!(deduped[0].locator(), "mssql://sa@host:1433/db");
        assert_eq!(deduped[0].backend(), BackendKind::SqlServer);
        assert_eq!(deduped[1].name(), "other");
    }

    #[test]
    fn sqlserver_tunnel_and_entra_round_trip_and_stay_optional() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("connections.toml");
        // A password-only entry still writes no tunnel/auth keys (FRE-57
        // files stay byte-compatible)…
        save_connections(&path, &[saved_ms("plain", "mssql://sa@h:1433/db")]).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(!text.contains("tunnel"));
        assert!(!text.contains("auth"));
        // …and an FRE-57-era minimal file (no tunnel/auth keys) deserializes.
        std::fs::write(
            &path,
            "[[connections]]\nkind = \"sqlserver\"\nname = \"ms\"\nurl = \"mssql://sa@h:1433/db\"\n",
        )
        .unwrap();
        assert_eq!(
            load_connections(&path).unwrap(),
            vec![saved_ms("ms", "mssql://sa@h:1433/db")]
        );
        // Tunnel + Entra round-trip with the same tagged shapes as Postgres.
        let entries = vec![SavedConnection::SqlServer {
            name: "azure sql".into(),
            url: "mssql://you@myserver.database.windows.net:1433/app?encrypt=on".into(),
            tunnel: Some(tunnel(crate::tunnel::TunnelAuth::Agent)),
            auth: PgAuth::Entra(EntraAuth::Interactive {
                tenant: "contoso.onmicrosoft.com".into(),
                client_id: None,
            }),
        }];
        save_connections(&path, &entries).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("kind = \"sqlserver\""));
        assert!(text.contains("method = \"agent\""));
        assert!(text.contains("kind = \"entra\""));
        assert_eq!(load_connections(&path).unwrap(), entries);
    }

    #[test]
    fn add_adopts_changed_sqlserver_tunnel_and_auth() {
        let dir = tempfile::tempdir().unwrap();
        let (mut list, _) = SavedList::load(&dir.path().join("connections.toml"));
        assert!(list.add(saved_ms("prod", "mssql://sa@h:1433/db")));
        // Same URL, tunnel + Entra added: the entry is updated, keeps its name.
        let updated = SavedConnection::SqlServer {
            name: "ignored".into(),
            url: "mssql://sa@h:1433/db".into(),
            tunnel: Some(tunnel(crate::tunnel::TunnelAuth::Agent)),
            auth: PgAuth::Entra(EntraAuth::interactive_default()),
        };
        assert!(list.add(updated.clone()));
        assert_eq!(list.entries().len(), 1);
        match &list.entries()[0] {
            SavedConnection::SqlServer {
                name, tunnel, auth, ..
            } => {
                assert_eq!(name, "prod");
                assert!(tunnel.is_some());
                assert!(matches!(auth, PgAuth::Entra(_)));
            }
            other => panic!("expected sqlserver, got {other:?}"),
        }
        // Identical settings again: no change.
        assert!(!list.add(updated));
    }

    #[test]
    fn plan_restore_treats_sqlserver_like_postgres() {
        let tabs = vec![
            SessionTab {
                locator: "mssql://sa@h:1433/withpw".into(),
                ..Default::default()
            },
            SessionTab {
                locator: "mssql://sa@h:1433/nopw".into(),
                ..Default::default()
            },
        ];
        let candidates = vec![
            RestoreCandidate {
                locator: "mssql://sa@h:1433/withpw".into(),
                backend: BackendKind::SqlServer,
            },
            RestoreCandidate {
                locator: "mssql://sa@h:1433/nopw".into(),
                backend: BackendKind::SqlServer,
            },
        ];
        let plan = plan_session_restore(&tabs, &candidates, |loc| loc.ends_with("withpw"));
        let locators: Vec<&str> = plan.iter().map(|t| t.locator.as_str()).collect();
        assert_eq!(locators, vec!["mssql://sa@h:1433/withpw"]);
    }

    #[test]
    fn add_dedupes_sqlserver_by_url() {
        let dir = tempfile::tempdir().unwrap();
        let (mut list, _) = SavedList::load(&dir.path().join("connections.toml"));
        assert!(list.add(saved_ms("prod", "mssql://sa@h:1433/db")));
        assert!(!list.add(saved_ms("other name", "mssql://sa@h:1433/db")));
        assert!(list.remove("mssql://sa@h:1433/db").is_some());
        assert!(list.entries().is_empty());
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
            saved_ms(
                "ms prod",
                "mssql://sa@db.example.com:1433/app?encrypt=on&trustServerCertificate=true",
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
                auth: PgAuth::Password,
            },
            SavedConnection::Postgres {
                name: "via key".into(),
                url: "postgres://u@db2.internal:5432/app".into(),
                tunnel: Some(tunnel(crate::tunnel::TunnelAuth::KeyFile {
                    path: PathBuf::from("/home/u/.ssh/id_ed25519"),
                })),
                auth: PgAuth::Password,
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
    fn password_connections_omit_the_auth_key_and_default_when_absent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("connections.toml");
        // A password connection writes no `auth` key (back-compat).
        save_connections(&path, &[saved_pg("prod", "postgres://u@h:5432/db")]).unwrap();
        assert!(!std::fs::read_to_string(&path).unwrap().contains("auth"));
        // And a file with no `auth` key loads as Password.
        assert_eq!(
            load_connections(&path).unwrap(),
            vec![saved_pg("prod", "postgres://u@h:5432/db")]
        );
    }

    #[test]
    fn entra_auth_modes_round_trip_through_the_config() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("connections.toml");
        let entries = vec![
            SavedConnection::Postgres {
                name: "azure-interactive".into(),
                url: "postgres://you@myserver.postgres.database.azure.com:5432/db".into(),
                tunnel: None,
                auth: PgAuth::Entra(EntraAuth::Interactive {
                    tenant: "contoso.onmicrosoft.com".into(),
                    client_id: None,
                }),
            },
            SavedConnection::Postgres {
                name: "azure-mi".into(),
                url: "postgres://mi@other.postgres.database.azure.com:5432/db".into(),
                tunnel: None,
                auth: PgAuth::Entra(EntraAuth::ManagedIdentity {
                    client_id: Some("11111111-2222-3333-4444-555555555555".into()),
                }),
            },
        ];
        save_connections(&path, &entries).unwrap();
        // The nested auth table carries kind="entra" + the method tag.
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("kind = \"entra\""));
        assert!(text.contains("method = \"interactive\""));
        assert!(text.contains("method = \"managedidentity\""));
        assert_eq!(load_connections(&path).unwrap(), entries);
    }

    #[test]
    fn add_adopts_a_changed_auth_mode() {
        let dir = tempfile::tempdir().unwrap();
        let (mut list, _) = SavedList::load(&dir.path().join("connections.toml"));
        assert!(list.add(saved_pg("prod", "postgres://u@h:5432/db")));
        // Same URL, now Entra: the entry is updated (and keeps its name).
        let entra = SavedConnection::Postgres {
            name: "ignored".into(),
            url: "postgres://u@h:5432/db".into(),
            tunnel: None,
            auth: PgAuth::Entra(EntraAuth::interactive_default()),
        };
        assert!(list.add(entra));
        assert_eq!(list.entries().len(), 1);
        match &list.entries()[0] {
            SavedConnection::Postgres { name, auth, .. } => {
                assert_eq!(name, "prod");
                assert!(matches!(auth, PgAuth::Entra(_)));
            }
            other => panic!("expected postgres, got {other:?}"),
        }
        // Re-adding the identical entry is a no-op.
        let same = SavedConnection::Postgres {
            name: "prod".into(),
            url: "postgres://u@h:5432/db".into(),
            tunnel: None,
            auth: PgAuth::Entra(EntraAuth::interactive_default()),
        };
        assert!(!list.add(same));
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
            auth: PgAuth::Password,
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
            let settings = Settings {
                theme,
                ..Default::default()
            };
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
        let settings = Settings {
            theme: Theme::Dark,
            ..Default::default()
        };
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

    #[test]
    fn window_geometry_round_trips_in_settings() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.toml");
        let settings = Settings {
            theme: Theme::Dark,
            window: Some(WindowGeometry {
                width: 1024.5,
                height: 768.0,
                x: Some(-40.0),
                y: Some(12.0),
                maximized: true,
            }),
        };
        save_settings(&path, &settings).unwrap();
        assert_eq!(load_settings(&path), settings);
    }

    #[test]
    fn missing_window_geometry_loads_as_none() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.toml");
        // A theme-only file (as written before FRE-30).
        std::fs::write(&path, "theme = \"light\"\n").unwrap();
        let loaded = load_settings(&path);
        assert_eq!(loaded.theme, Theme::Light);
        assert_eq!(loaded.window, None);
    }

    #[test]
    fn saving_geometry_preserves_theme_and_vice_versa() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.toml");
        save_theme(&path, Theme::Dark).unwrap();
        let geo = WindowGeometry {
            width: 900.0,
            height: 600.0,
            x: Some(10.0),
            y: Some(20.0),
            maximized: false,
        };
        save_window_geometry(&path, geo).unwrap();
        // The geometry write must not clobber the theme…
        assert_eq!(load_settings(&path).theme, Theme::Dark);
        // …and a later theme write must not clobber the geometry.
        save_theme(&path, Theme::Light).unwrap();
        let loaded = load_settings(&path);
        assert_eq!(loaded.theme, Theme::Light);
        assert_eq!(loaded.window, Some(geo));
    }

    #[test]
    fn geometry_sanitized_clamps_tiny_huge_and_negative_sizes() {
        // Tiny/negative sizes clamp up to the minimums.
        let tiny = WindowGeometry {
            width: 1.0,
            height: -50.0,
            x: Some(5.0),
            y: Some(5.0),
            maximized: false,
        }
        .sanitized();
        assert_eq!(tiny.width, MIN_WINDOW_WIDTH);
        assert_eq!(tiny.height, MIN_WINDOW_HEIGHT);
        assert_eq!(tiny.x, Some(5.0));

        // Huge sizes clamp down to the maximum.
        let huge = WindowGeometry {
            width: 1.0e9,
            height: 1.0e9,
            x: None,
            y: None,
            maximized: false,
        }
        .sanitized();
        assert_eq!(huge.width, MAX_WINDOW_DIM);
        assert_eq!(huge.height, MAX_WINDOW_DIM);

        // Non-finite sizes fall back to the launch defaults; a non-finite or
        // wildly out-of-range position is dropped.
        let broken = WindowGeometry {
            width: f64::NAN,
            height: f64::INFINITY,
            x: Some(f64::NAN),
            y: Some(1.0e9),
            maximized: true,
        }
        .sanitized();
        assert_eq!(broken.width, DEFAULT_WINDOW_WIDTH);
        assert_eq!(broken.height, DEFAULT_WINDOW_HEIGHT);
        assert_eq!(broken.x, None);
        assert_eq!(broken.y, None);
        assert!(broken.maximized);

        // A reasonable geometry (including a negative multi-monitor x) is
        // left untouched.
        let ok = WindowGeometry {
            width: 1000.0,
            height: 700.0,
            x: Some(-100.0),
            y: Some(50.0),
            maximized: false,
        };
        assert_eq!(ok.sanitized(), ok);
    }

    #[test]
    fn missing_session_file_loads_empty() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nope").join("session.toml");
        assert_eq!(load_session(&path), Session::default());
        assert!(load_session(&path).tabs.is_empty());
    }

    #[test]
    fn malformed_session_file_loads_empty() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.toml");
        std::fs::write(&path, "tabs = \"not a list\"").unwrap();
        assert_eq!(load_session(&path), Session::default());
    }

    #[test]
    fn session_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("deep").join("session.toml");
        let session = Session {
            tabs: vec![
                SessionTab {
                    locator: "/data/music.db".into(),
                    selected_schema: None,
                    selected_table: Some("artists".into()),
                    pane: SessionPane::Browser,
                },
                SessionTab {
                    locator: "postgres://u@h:5432/app".into(),
                    selected_schema: Some("public".into()),
                    selected_table: Some("orders".into()),
                    pane: SessionPane::Sql,
                },
                SessionTab {
                    locator: "postgres://u@h:5432/other".into(),
                    selected_schema: None,
                    selected_table: Some("stock".into()),
                    pane: SessionPane::Schema,
                },
            ],
            active: Some("postgres://u@h:5432/app".into()),
        };
        save_session(&path, &session).unwrap();
        assert_eq!(load_session(&path), session);
    }

    #[test]
    fn session_without_a_pane_key_loads_as_the_default() {
        // Files written before panes were persisted at all.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.toml");
        std::fs::write(
            &path,
            "active = \"/data/music.db\"\n\n[[tabs]]\nlocator = \"/data/music.db\"\n",
        )
        .unwrap();
        let session = load_session(&path);
        assert_eq!(session.tabs.len(), 1);
        assert_eq!(session.tabs[0].pane, SessionPane::Browser);
    }

    #[test]
    fn an_unknown_pane_keeps_the_rest_of_the_session() {
        // A pane added by a newer build must not cost this one every tab:
        // serde treats an unknown enum variant as a hard error, and
        // `load_session` discards the whole file on any parse failure.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.toml");
        std::fs::write(
            &path,
            "active = \"/data/music.db\"\n\n[[tabs]]\nlocator = \"/data/music.db\"\n\
             selected_table = \"artists\"\npane = \"telemetry\"\n",
        )
        .unwrap();
        let session = load_session(&path);
        assert_eq!(session.tabs.len(), 1, "the tab must survive");
        assert_eq!(session.tabs[0].selected_table.as_deref(), Some("artists"));
        assert_eq!(session.tabs[0].pane, SessionPane::Browser);
        assert_eq!(session.active.as_deref(), Some("/data/music.db"));
    }

    #[test]
    fn plan_restore_keeps_sqlite_and_password_backed_postgres() {
        let tabs = vec![
            SessionTab {
                locator: "/data/a.db".into(),
                selected_table: Some("t".into()),
                ..Default::default()
            },
            SessionTab {
                locator: "postgres://u@h:5432/withpw".into(),
                ..Default::default()
            },
            SessionTab {
                locator: "postgres://u@h:5432/nopw".into(),
                ..Default::default()
            },
            SessionTab {
                locator: "/data/gone.db".into(),
                ..Default::default()
            },
        ];
        let candidates = vec![
            RestoreCandidate {
                locator: "/data/a.db".into(),
                backend: BackendKind::Sqlite,
            },
            RestoreCandidate {
                locator: "postgres://u@h:5432/withpw".into(),
                backend: BackendKind::Postgres,
            },
            RestoreCandidate {
                locator: "postgres://u@h:5432/nopw".into(),
                backend: BackendKind::Postgres,
            },
            // "/data/gone.db" is in the session but NOT saved anymore.
        ];
        let plan = plan_session_restore(&tabs, &candidates, |loc| loc.ends_with("withpw"));
        let locators: Vec<&str> = plan.iter().map(|t| t.locator.as_str()).collect();
        // SQLite kept, pg-with-password kept, pg-without-password skipped, and
        // the no-longer-saved sqlite dropped. Order preserved.
        assert_eq!(locators, vec!["/data/a.db", "postgres://u@h:5432/withpw"]);
    }
}
