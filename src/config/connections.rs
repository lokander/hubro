//! The saved-connections list: `$XDG_CONFIG_HOME/hubro/connections.toml`.
//!
//! Error policy: **strict**. A malformed file is an error, never silently
//! dropped — this file is user data (the list of their databases), and
//! [`SavedList`] refuses to persist over a file that failed to load so a
//! parse error can't turn into data loss on the next save.

use std::fmt;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::{write_toml_atomic, ConfigError};
use crate::azure::EntraAuth;
use crate::db::{Dialect, WriteProtection};
use crate::tunnel::TunnelConfig;

/// A connection's accent colour (FRE-111): a warning you can see from across
/// the room, on the tab, the sidebar and the connections list.
///
/// Deliberately a fixed palette rather than a free-form colour string. The
/// colour is free-form in the sense the issue meant — it encodes no
/// environment, so nothing here is named "production" and a red connection
/// carries no behaviour — but the *values* are closed, which keeps the swatch
/// picker simple and keeps user-supplied text out of the inline `style`
/// attributes these render into.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ConnectionColor {
    Red,
    Amber,
    Green,
    Blue,
    Purple,
    Gray,
}

impl ConnectionColor {
    /// Every colour, in picker order.
    pub const ALL: [ConnectionColor; 6] = [
        ConnectionColor::Red,
        ConnectionColor::Amber,
        ConnectionColor::Green,
        ConnectionColor::Blue,
        ConnectionColor::Purple,
        ConnectionColor::Gray,
    ];

    /// The CSS colour these render as. One mid-tone per hue, chosen to stay
    /// legible against both the light and the dark theme rather than needing a
    /// per-theme pair.
    pub fn css(self) -> &'static str {
        match self {
            ConnectionColor::Red => "#dc2626",
            ConnectionColor::Amber => "#d97706",
            ConnectionColor::Green => "#059669",
            ConnectionColor::Blue => "#2563eb",
            ConnectionColor::Purple => "#7c3aed",
            ConnectionColor::Gray => "#6b7280",
        }
    }

    /// The name shown in the picker's tooltip.
    pub fn label(self) -> &'static str {
        match self {
            ConnectionColor::Red => "Red",
            ConnectionColor::Amber => "Amber",
            ConnectionColor::Green => "Green",
            ConnectionColor::Blue => "Blue",
            ConnectionColor::Purple => "Purple",
            ConnectionColor::Gray => "Gray",
        }
    }
}

/// How a server connection (Postgres, FRE-43; SQL Server, FRE-58)
/// authenticates. `Password` (the default) resolves a password from session
/// memory / the keyring / a prompt; `Entra` acquires a Microsoft Entra ID
/// access token and uses it in place of the password.
///
/// Serialized by contents (the `kind = "entra"` tag), so the type's Rust
/// name never reaches the TOML — renaming it from `PgAuth` (the backend it
/// landed on first) was on-disk-safe.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum ServerAuth {
    #[default]
    Password,
    Entra(EntraAuth),
}

impl ServerAuth {
    /// Whether this is the default password mode — lets the config skip writing
    /// an `[…auth]` key for ordinary connections (back-compat).
    pub fn is_password(&self) -> bool {
        matches!(self, ServerAuth::Password)
    }
}

/// One entry in the saved-connections list. Internally tagged on `kind`, so
/// existing `kind = "sqlite"` + `path` TOML entries keep deserializing.
///
/// The `protection` and `color` fields (FRE-111) and `group` (FRE-120) repeat
/// on every variant rather than being hoisted into a shared struct: an
/// internally-tagged enum can't `#[serde(flatten)]` one in without changing
/// the on-disk shape, and all three serialize as plain strings, so they stay
/// safe to place anywhere in a TOML table. Read them through
/// [`SavedConnection::protection`], [`SavedConnection::color`] and
/// [`SavedConnection::group`] rather than matching.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum SavedConnection {
    Sqlite {
        name: String,
        path: PathBuf,
        /// How much this connection resists writes (FRE-111). `default` +
        /// `skip_serializing_if` keep pre-FRE-111 config files deserializing
        /// and unprotected entries' TOML unchanged; a missing key is `Open`.
        #[serde(default, skip_serializing_if = "WriteProtection::is_open")]
        protection: WriteProtection,
        /// Accent colour (FRE-111), stored exactly like `protection`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        color: Option<ConnectionColor>,
        /// The group this connection is filed under (FRE-120), by name, or
        /// `None` for ungrouped. One name, not a list: a connection belongs
        /// to at most one group. Stored exactly like `color`, so pre-FRE-120
        /// files load and ungrouped entries' TOML is unchanged.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        group: Option<String>,
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
        #[serde(default, skip_serializing_if = "ServerAuth::is_password")]
        auth: ServerAuth,
        /// Write protection (FRE-111); see the `Sqlite` variant.
        #[serde(default, skip_serializing_if = "WriteProtection::is_open")]
        protection: WriteProtection,
        /// Accent colour (FRE-111); see the `Sqlite` variant.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        color: Option<ConnectionColor>,
        /// Group membership (FRE-120); see the `Sqlite` variant.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        group: Option<String>,
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
        #[serde(default, skip_serializing_if = "ServerAuth::is_password")]
        auth: ServerAuth,
        /// Write protection (FRE-111); see the `Sqlite` variant.
        #[serde(default, skip_serializing_if = "WriteProtection::is_open")]
        protection: WriteProtection,
        /// Accent colour (FRE-111); see the `Sqlite` variant.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        color: Option<ConnectionColor>,
        /// Group membership (FRE-120); see the `Sqlite` variant.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        group: Option<String>,
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

    /// How much this connection resists writes (FRE-111).
    pub fn protection(&self) -> WriteProtection {
        match self {
            SavedConnection::Sqlite { protection, .. }
            | SavedConnection::Postgres { protection, .. }
            | SavedConnection::SqlServer { protection, .. } => *protection,
        }
    }

    /// This connection's accent colour (FRE-111), if the user set one.
    pub fn color(&self) -> Option<ConnectionColor> {
        match self {
            SavedConnection::Sqlite { color, .. }
            | SavedConnection::Postgres { color, .. }
            | SavedConnection::SqlServer { color, .. } => *color,
        }
    }

    /// The group this connection is filed under (FRE-120), or `None` when it
    /// is ungrouped.
    pub fn group(&self) -> Option<&str> {
        match self {
            SavedConnection::Sqlite { group, .. }
            | SavedConnection::Postgres { group, .. }
            | SavedConnection::SqlServer { group, .. } => group.as_deref(),
        }
    }

    /// Files this connection under `group` (FRE-120), or ungroups it with
    /// `None`. Assigning replaces rather than adds: the field holds one name,
    /// which is what makes "at most one group" a property of the type instead
    /// of a rule every caller has to remember.
    pub fn set_group(&mut self, new_group: Option<String>) {
        match self {
            SavedConnection::Sqlite { group, .. }
            | SavedConnection::Postgres { group, .. }
            | SavedConnection::SqlServer { group, .. } => *group = new_group,
        }
    }

    /// Adopts `other`'s group when this entry has none — the group half of
    /// the same "two entries collapse into one" rule as
    /// [`Self::merge_marking_from`].
    ///
    /// Kept separate because the rule differs. Protection takes the *stricter*
    /// of the two, since one side of that trade is losing protection the user
    /// asked for; a group has no strict/loose ordering, so the survivor's own
    /// filing wins and the other's only fills a gap. The shared part is that
    /// neither may silently become `None`: a connection that quietly leaves
    /// its group between one launch and the next is exactly as confusing as
    /// one that quietly leaves its colour behind.
    pub fn merge_group_from(&mut self, other: &SavedConnection) {
        if self.group().is_none() {
            self.set_group(other.group().map(str::to_string));
        }
    }

    /// Folds `other`'s marking into this one, keeping whichever is stricter.
    ///
    /// The one merge rule, shared by every path where two entries collapse
    /// into one: an edit that absorbs a duplicate ([`SavedList::update`]) and
    /// the load-time dedup ([`normalize_and_dedup`]). Both used to keep the
    /// survivor's marking and discard the other's, which is how a read-only
    /// `prod` entry could vanish into an unmarked one still addressing
    /// production — protection lost with the row that carried it.
    ///
    /// Stricter wins because the asymmetry is not close: keeping protection
    /// the user no longer needs costs them one click, and dropping protection
    /// they do need costs them the thing this whole feature exists to prevent.
    pub fn merge_marking_from(&mut self, other: &SavedConnection) {
        self.set_marking(
            self.protection().max(other.protection()),
            self.color().or(other.color()),
        );
    }

    /// Replaces the protection and colour, leaving everything else alone —
    /// the one write path for FRE-111's two fields, so no caller has to
    /// reconstruct a variant just to re-mark it.
    pub fn set_marking(
        &mut self,
        new_protection: WriteProtection,
        new_color: Option<ConnectionColor>,
    ) {
        match self {
            SavedConnection::Sqlite {
                protection, color, ..
            }
            | SavedConnection::Postgres {
                protection, color, ..
            }
            | SavedConnection::SqlServer {
                protection, color, ..
            } => {
                *protection = new_protection;
                *color = new_color;
            }
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

/// A saved connection's URL split back into the form's individual fields
/// (FRE-75). Both backends share the `scheme://user@host:port/db?opt=…`
/// shape, so one splitter serves both; `option_key` names the query
/// parameter the form's own dropdown owns (`sslmode` / `encrypt`).
struct UrlFields {
    host: String,
    port: String,
    database: String,
    user: String,
    option: Option<String>,
    trust_cert: bool,
}

fn split_url(url: &str, option_key: &str) -> Option<UrlFields> {
    let parsed = url::Url::parse(url).ok()?;
    let mut option = None;
    let mut trust_cert = false;
    // Query keys are compared case-insensitively: the app writes
    // `trustServerCertificate`, and a hand-pasted URL may use any casing.
    let option_key = option_key.to_ascii_lowercase();
    for (key, value) in parsed.query_pairs() {
        let key = key.to_ascii_lowercase();
        if key == option_key {
            option = Some(value.into_owned());
        } else if key == "trustservercertificate" {
            // Same spellings the SQL Server driver accepts.
            trust_cert = matches!(value.to_ascii_lowercase().as_str(), "true" | "yes" | "1");
        }
    }
    Some(UrlFields {
        host: parsed.host_str().unwrap_or_default().to_string(),
        port: parsed.port().map(|p| p.to_string()).unwrap_or_default(),
        database: parsed.path().trim_start_matches('/').to_string(),
        user: percent_decode(parsed.username()),
        option,
        trust_cert,
    })
}

/// Percent-decodes a URL component back to what the user typed into the
/// field (the url crate encodes on the way in).
fn percent_decode(raw: &str) -> String {
    percent_encoding::percent_decode_str(raw)
        .decode_utf8()
        .map(|s| s.into_owned())
        .unwrap_or_else(|_| raw.to_string())
}

/// A saved entry decomposed into the connection forms' field values
/// (FRE-75). Secrets are deliberately absent: the password and SSH
/// passphrase fields always start empty, and an empty field on save means
/// "keep whatever is in the keyring".
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct EditPrefill {
    pub name: String,
    pub host: String,
    pub port: String,
    pub database: String,
    pub user: String,
    /// `sslmode` for Postgres, `encrypt` for SQL Server.
    pub option: Option<String>,
    pub trust_cert: bool,
    pub auth_mode: String,
    pub entra_tenant: String,
    pub entra_client_id: String,
    pub tunnel: Option<TunnelConfig>,
    pub ssh_host: String,
    pub ssh_port: String,
    pub ssh_user: String,
    pub ssh_use_key: bool,
    pub ssh_key_path: String,
}

impl Default for EditPrefill {
    /// The add flow's starting point. Note `auth_mode`/`entra_tenant` are
    /// the forms' real defaults rather than empty strings — the password
    /// field only renders while `auth_mode` is "password".
    fn default() -> Self {
        EditPrefill {
            name: String::new(),
            host: String::new(),
            port: String::new(),
            database: String::new(),
            user: String::new(),
            option: None,
            trust_cert: false,
            auth_mode: "password".to_string(),
            entra_tenant: "organizations".to_string(),
            entra_client_id: String::new(),
            tunnel: None,
            ssh_host: String::new(),
            ssh_port: String::new(),
            ssh_user: String::new(),
            ssh_use_key: false,
            ssh_key_path: String::new(),
        }
    }
}

impl EditPrefill {
    /// Decomposes a saved entry into the form's field values.
    pub fn from_saved(saved: SavedConnection) -> Self {
        use crate::tunnel::TunnelAuth;
        let (name, url, tunnel, auth, option_key) = match saved {
            SavedConnection::Postgres {
                name,
                url,
                tunnel,
                auth,
                ..
            } => (name, url, tunnel, auth, "sslmode"),
            SavedConnection::SqlServer {
                name,
                url,
                tunnel,
                auth,
                ..
            } => (name, url, tunnel, auth, "encrypt"),
            // SQLite entries carry only a path; they have no edit form.
            SavedConnection::Sqlite { name, .. } => {
                return EditPrefill {
                    name,
                    ..EditPrefill::default()
                }
            }
        };
        let fields = split_url(&url, option_key);
        let (auth_mode, entra_tenant, entra_client_id) = match auth {
            ServerAuth::Password => ("password".into(), "organizations".into(), String::new()),
            ServerAuth::Entra(EntraAuth::Interactive { tenant, client_id }) => (
                "entra-interactive".to_string(),
                tenant,
                client_id.unwrap_or_default(),
            ),
            ServerAuth::Entra(EntraAuth::ManagedIdentity { client_id }) => (
                "entra-mi".to_string(),
                "organizations".to_string(),
                client_id.unwrap_or_default(),
            ),
        };
        let (ssh_use_key, ssh_key_path) = match tunnel.as_ref().map(|t| &t.auth) {
            Some(TunnelAuth::KeyFile { path }) => (true, path.display().to_string()),
            _ => (false, String::new()),
        };
        EditPrefill {
            name,
            host: fields.as_ref().map(|f| f.host.clone()).unwrap_or_default(),
            port: fields.as_ref().map(|f| f.port.clone()).unwrap_or_default(),
            database: fields
                .as_ref()
                .map(|f| f.database.clone())
                .unwrap_or_default(),
            user: fields.as_ref().map(|f| f.user.clone()).unwrap_or_default(),
            option: fields.as_ref().and_then(|f| f.option.clone()),
            trust_cert: fields.as_ref().is_some_and(|f| f.trust_cert),
            auth_mode,
            entra_tenant,
            entra_client_id,
            ssh_host: tunnel.as_ref().map(|t| t.host.clone()).unwrap_or_default(),
            ssh_port: tunnel
                .as_ref()
                .map(|t| t.port.to_string())
                .unwrap_or_default(),
            ssh_user: tunnel.as_ref().map(|t| t.user.clone()).unwrap_or_default(),
            ssh_use_key,
            ssh_key_path,
            tunnel,
        }
    }
}

/// The whole connections file: the group list (FRE-120) and the entries.
///
/// `groups` is declared first because TOML has to emit a plain array before
/// the `[[connections]]` tables, and it exists at all — rather than being
/// derived from the entries' `group` fields — for two reasons a derived list
/// can't cover: it fixes the **display order** of the groups, and it lets an
/// **empty group** exist. A group created and not yet filled would otherwise
/// vanish the moment it was written.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct ConnectionsFile {
    /// Group names in display order (FRE-120). `default` +
    /// `skip_serializing_if` keep pre-FRE-120 files loading, and a file with
    /// no groups is written exactly as before.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub groups: Vec<String>,
    #[serde(default)]
    pub connections: Vec<SavedConnection>,
}

/// Default location: `$XDG_CONFIG_HOME/hubro/connections.toml`.
pub fn default_config_path() -> Option<PathBuf> {
    Some(dirs::config_dir()?.join("hubro").join("connections.toml"))
}

/// Loads the whole file — groups and entries. A missing file is an empty
/// file, not an error; a malformed one is an error (don't silently drop user
/// data).
pub fn load_connections_file(path: &Path) -> Result<ConnectionsFile, ConfigError> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Ok(ConnectionsFile::default())
        }
        Err(err) => return Err(ConfigError(format!("reading {}: {err}", path.display()))),
    };
    toml::from_str(&text).map_err(|err| ConfigError(format!("{}: {err}", path.display())))
}

/// Saves the whole file, creating parent directories as needed. Writes to a
/// temp file and renames so a crash mid-write can't corrupt the config.
pub fn save_connections_file(path: &Path, file: &ConnectionsFile) -> Result<(), ConfigError> {
    write_toml_atomic(path, file)
}

/// Loads just the saved connections, for callers with no interest in the
/// group list.
pub fn load_connections(path: &Path) -> Result<Vec<SavedConnection>, ConfigError> {
    Ok(load_connections_file(path)?.connections)
}

/// Saves a group-less list — the shape of the file before FRE-120, and what
/// a caller holding only entries can write.
pub fn save_connections(path: &Path, connections: &[SavedConnection]) -> Result<(), ConfigError> {
    save_connections_file(
        path,
        &ConnectionsFile {
            groups: Vec::new(),
            connections: connections.to_vec(),
        },
    )
}

/// Why a group name was refused (FRE-120). Both cases are user input from
/// the connections screen, so each carries the sentence shown there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GroupError {
    /// The name was empty, or only whitespace.
    Empty,
    /// Another group already has this name (compared case-insensitively).
    Duplicate(String),
}

impl fmt::Display for GroupError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            GroupError::Empty => write!(f, "a group needs a name"),
            GroupError::Duplicate(name) => write!(f, "there is already a group called “{name}”"),
        }
    }
}

impl std::error::Error for GroupError {}

/// One section of the connections list as it is rendered (FRE-120): a group
/// and the connections filed under it, or — with `name` as `None` — the
/// ungrouped ones.
#[derive(Debug, Clone, PartialEq)]
pub struct GroupSection {
    pub name: Option<String>,
    pub entries: Vec<SavedConnection>,
}

/// Whether a connection's name matches a search box's contents (FRE-120):
/// case-insensitive substring, with an empty (or whitespace-only) query
/// matching everything so the unfiltered list renders through the same path.
///
/// Deliberately the *name* alone, which is what the issue asked for and what
/// the box says it does. Matching the URL or the group name as well would
/// make hits appear that the user can't see the reason for.
pub fn name_matches(name: &str, query: &str) -> bool {
    let query = query.trim();
    if query.is_empty() {
        return true;
    }
    name.to_lowercase().contains(&query.to_lowercase())
}

/// Whether two group names are the same name (FRE-120).
///
/// Identity is exact — that is what a connection's `group` field is matched
/// against — but *creating* or *renaming* compares case-insensitively, so
/// "Prod" and "prod" can't both be made through the UI. A hand-edited file
/// can still hold both, and then both are shown: reconciling them by folding
/// one into the other would move connections the user never asked to move.
fn same_group_name(a: &str, b: &str) -> bool {
    a.to_lowercase() == b.to_lowercase()
}

/// The saved-connections list plus the load outcome. Mutations go through
/// this type so a list that failed to load is never persisted back over the
/// user's file.
#[derive(Debug, Clone, PartialEq)]
pub struct SavedList {
    entries: Vec<SavedConnection>,
    /// Group names in display order (FRE-120).
    groups: Vec<String>,
    load_failed: bool,
}

/// Canonicalizes Postgres (FRE-39) and SQL Server URLs and drops entries that
/// collapse to an already-present locator — upgrading a list saved before
/// normalization existed (e.g. `postgresql://` or a portless URL) and
/// de-duplicating it. The first entry for each locator wins, so its name is
/// kept.
///
/// A dropped duplicate's **marking survives** into the entry that absorbs it
/// ([`SavedConnection::merge_marking_from`], FRE-111). This dedup compares
/// *normalized* URLs while `SavedList::update` compares the stored locator
/// string, so two entries can coexist all session and only collapse at the
/// next launch — discarding the loser's marking outright would silently
/// unprotect a connection between one run and the next.
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
        match out
            .iter_mut()
            .find(|existing| existing.locator() == locator)
        {
            Some(kept) => {
                kept.merge_marking_from(&entry);
                kept.merge_group_from(&entry);
            }
            None => out.push(entry),
        }
    }
    out
}

/// Reconciles the entries' group names with the group list (FRE-120), so the
/// one invariant the rest of this module relies on holds however the file was
/// written: **every group an entry names is in `groups`**.
///
/// Trims each entry's name and treats an empty one as ungrouped, then appends
/// any group named by an entry but missing from the list, in the order the
/// entries appear. A hand-edited file that files a connection under a group
/// it never declared would otherwise render that connection nowhere — the
/// list is drawn from `groups`, so a section that doesn't exist can't show
/// its members.
fn reconcile_groups(groups: &mut Vec<String>, entries: &mut [SavedConnection]) {
    for name in groups.iter_mut() {
        *name = name.trim().to_string();
    }
    groups.retain(|name| !name.is_empty());
    for entry in entries.iter_mut() {
        let trimmed = entry.group().map(str::trim).unwrap_or_default().to_string();
        if trimmed.is_empty() {
            entry.set_group(None);
            continue;
        }
        entry.set_group(Some(trimmed.clone()));
        if !groups.contains(&trimmed) {
            groups.push(trimmed);
        }
    }
}

impl SavedList {
    pub fn load(path: &Path) -> (Self, Option<ConfigError>) {
        match load_connections_file(path) {
            Ok(file) => {
                let mut entries = normalize_and_dedup(file.connections);
                let mut groups = file.groups;
                reconcile_groups(&mut groups, &mut entries);
                (
                    Self {
                        entries,
                        groups,
                        load_failed: false,
                    },
                    None,
                )
            }
            Err(err) => (
                Self {
                    entries: Vec::new(),
                    groups: Vec::new(),
                    load_failed: true,
                },
                Some(err),
            ),
        }
    }

    pub fn entries(&self) -> &[SavedConnection] {
        &self.entries
    }

    /// The groups (FRE-120) in display order.
    pub fn groups(&self) -> &[String] {
        &self.groups
    }

    /// Creates a group, appended last so a new one appears where it was made
    /// rather than jumping the order. Returns the trimmed name it was
    /// created under.
    pub fn create_group(&mut self, name: &str) -> Result<String, GroupError> {
        let name = name.trim().to_string();
        if name.is_empty() {
            return Err(GroupError::Empty);
        }
        if let Some(existing) = self.groups.iter().find(|g| same_group_name(g, &name)) {
            return Err(GroupError::Duplicate(existing.clone()));
        }
        self.groups.push(name.clone());
        Ok(name)
    }

    /// Renames a group, carrying its members with it.
    ///
    /// Membership is stored as the group's name, so the rename has to rewrite
    /// every member in the same step — a rename that renamed only the header
    /// would empty the group and strand its connections under a name nothing
    /// displays. Returns the trimmed new name (validated even when `old`
    /// names no group, in which case nothing is renamed).
    pub fn rename_group(&mut self, old: &str, new: &str) -> Result<String, GroupError> {
        let new = new.trim().to_string();
        if new.is_empty() {
            return Err(GroupError::Empty);
        }
        // A case-only change of the group's own name is a rename, not a
        // collision with itself.
        if let Some(existing) = self
            .groups
            .iter()
            .find(|g| g.as_str() != old && same_group_name(g, &new))
        {
            return Err(GroupError::Duplicate(existing.clone()));
        }
        let Some(index) = self.groups.iter().position(|g| g == old) else {
            return Ok(new);
        };
        self.groups[index] = new.clone();
        for entry in &mut self.entries {
            if entry.group() == Some(old) {
                entry.set_group(Some(new.clone()));
            }
        }
        Ok(new)
    }

    /// Removes a group; its members become ungrouped rather than being
    /// removed with it — deleting a folder must never delete the databases in
    /// it. Returns whether anything changed.
    pub fn remove_group(&mut self, name: &str) -> bool {
        let Some(index) = self.groups.iter().position(|g| g == name) else {
            return false;
        };
        self.groups.remove(index);
        for entry in &mut self.entries {
            if entry.group() == Some(name) {
                entry.set_group(None);
            }
        }
        true
    }

    /// Moves a group one step up or down the display order — the whole of
    /// "reorder", expressed as single steps because that is what a pair of
    /// buttons can drive without a drag-and-drop dependency. Returns false at
    /// the ends of the list (and for an unknown name), which is what greys
    /// the button out.
    pub fn move_group(&mut self, name: &str, up: bool) -> bool {
        let Some(index) = self.groups.iter().position(|g| g == name) else {
            return false;
        };
        let target = if up {
            match index.checked_sub(1) {
                Some(target) => target,
                None => return false,
            }
        } else if index + 1 < self.groups.len() {
            index + 1
        } else {
            return false;
        };
        self.groups.swap(index, target);
        true
    }

    /// Files the connection at `locator` under `group` (FRE-120), or
    /// ungroups it with `None`. Assigning replaces any previous group, so a
    /// connection is never in two.
    ///
    /// An unknown group name assigns nothing and returns false: the only way
    /// to reach one is a stale UI, and inventing the group instead would let
    /// a typo create one silently. Returns false when nothing changed, so the
    /// caller can skip a write to disk.
    pub fn assign_group(&mut self, locator: &str, group: Option<&str>) -> bool {
        if let Some(name) = group {
            if !self.groups.iter().any(|g| g == name) {
                return false;
            }
        }
        let Some(entry) = self.entries.iter_mut().find(|s| s.locator() == locator) else {
            return false;
        };
        if entry.group() == group {
            return false;
        }
        entry.set_group(group.map(str::to_string));
        true
    }

    /// The list arranged for display (FRE-120): every group in order with the
    /// connections filed under it, then the ungrouped ones last, narrowed to
    /// the connections whose name matches `query` ([`name_matches`]).
    ///
    /// Two rules the callers would otherwise each have to get right:
    ///
    /// - An **empty group is kept** while the query is empty — it is a thing
    ///   the user made, and a group that disappeared until it had a member
    ///   could not be filled in the first place. Under a search it is
    ///   dropped, along with every other section that matched nothing, so the
    ///   result reads as hits rather than as headers.
    /// - The ungrouped section is omitted when it is empty, so a fully
    ///   grouped list grows no stray "Ungrouped" header.
    pub fn arrange(&self, query: &str) -> Vec<GroupSection> {
        let searching = !query.trim().is_empty();
        let mut sections: Vec<GroupSection> = Vec::with_capacity(self.groups.len() + 1);
        for group in &self.groups {
            let entries: Vec<SavedConnection> = self
                .entries
                .iter()
                .filter(|e| e.group() == Some(group.as_str()) && name_matches(e.name(), query))
                .cloned()
                .collect();
            if searching && entries.is_empty() {
                continue;
            }
            sections.push(GroupSection {
                name: Some(group.clone()),
                entries,
            });
        }
        let ungrouped: Vec<SavedConnection> = self
            .entries
            .iter()
            .filter(|e| e.group().is_none() && name_matches(e.name(), query))
            .cloned()
            .collect();
        if !ungrouped.is_empty() {
            sections.push(GroupSection {
                name: None,
                entries: ungrouped,
            });
        }
        sections
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

    /// Replaces the entry at `old_locator` with `connection`, keeping its
    /// position in the list (FRE-75). Unlike [`Self::add`], this overwrites
    /// every field — including the display name — which is the whole point
    /// of an edit. When the edit moves the entry onto another entry's
    /// locator, that duplicate is dropped so the list stays keyed one-to-one.
    /// Returns false when `old_locator` names no entry.
    ///
    /// **The write protection and colour (FRE-111) and the group (FRE-120)
    /// survive the overwrite.** None of them are part of what the edit form
    /// collects, so `connection` always carries the defaults for them —
    /// taking those literally would silently unprotect a connection, and
    /// unfile it, the moment its name or sslmode was edited, and the loss
    /// would only show up at the next launch. No caller of `update` can
    /// intend to change either; the paths that do are [`Self::set_marking`]
    /// and [`Self::assign_group`].
    ///
    /// When the edit collides with another entry, the surviving entry takes
    /// the **stricter** of the two markings. Repointing an unprotected
    /// `staging` at production's URL would otherwise absorb the read-only
    /// entry and leave an `Open` one addressing the same database — the
    /// protection would vanish along with the row that carried it.
    pub fn update(&mut self, old_locator: &str, mut connection: SavedConnection) -> bool {
        let Some(index) = self.entries.iter().position(|s| s.locator() == old_locator) else {
            return false;
        };
        let new_locator = connection.locator().to_string();
        connection.set_marking(
            self.entries[index].protection(),
            self.entries[index].color(),
        );
        connection.set_group(self.entries[index].group().map(str::to_string));
        // Fold in the marking and group of any entry this edit is about to
        // absorb.
        for i in 0..self.entries.len() {
            if i != index && self.entries[i].locator() == new_locator {
                let absorbed = self.entries[i].clone();
                connection.merge_marking_from(&absorbed);
                connection.merge_group_from(&absorbed);
            }
        }
        self.entries[index] = connection;
        // Drop any *other* entry the edit now collides with, keeping the one
        // just written.
        let mut position = 0;
        self.entries.retain(|s| {
            let keep = position == index || s.locator() != new_locator;
            position += 1;
            keep
        });
        true
    }

    /// Re-marks one entry's write protection and accent colour (FRE-111).
    ///
    /// Separate from [`Self::update`] because marking is not an edit of the
    /// connection: it touches neither the locator nor anything that could
    /// collide with another entry, so it can't reorder or drop rows. Returns
    /// false when `locator` names no entry, or when nothing changed — the
    /// caller uses that to skip a pointless write to disk.
    pub fn set_marking(
        &mut self,
        locator: &str,
        protection: WriteProtection,
        color: Option<ConnectionColor>,
    ) -> bool {
        let Some(entry) = self.entries.iter_mut().find(|s| s.locator() == locator) else {
            return false;
        };
        if entry.protection() == protection && entry.color() == color {
            return false;
        }
        entry.set_marking(protection, color);
        true
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
        save_connections_file(
            path,
            &ConnectionsFile {
                groups: self.groups.clone(),
                connections: self.entries.clone(),
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn saved(name: &str, path: &str) -> SavedConnection {
        SavedConnection::Sqlite {
            name: name.into(),
            path: PathBuf::from(path),
            protection: WriteProtection::Open,
            color: None,
            group: None,
        }
    }

    fn saved_pg(name: &str, url: &str) -> SavedConnection {
        SavedConnection::Postgres {
            name: name.into(),
            url: url.into(),
            tunnel: None,
            auth: ServerAuth::Password,
            protection: WriteProtection::Open,
            color: None,
            group: None,
        }
    }

    fn saved_ms(name: &str, url: &str) -> SavedConnection {
        SavedConnection::SqlServer {
            name: name.into(),
            url: url.into(),
            tunnel: None,
            auth: ServerAuth::Password,
            protection: WriteProtection::Open,
            color: None,
            group: None,
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
    fn a_connection_belongs_to_at_most_one_group() {
        // Assigning replaces: there is one field, and the second assignment
        // has to move the connection rather than add to it.
        let mut list = list_of(&[saved("a", "/tmp/a.db")]);
        assert_eq!(list.create_group("Production").unwrap(), "Production");
        assert_eq!(list.create_group("Staging").unwrap(), "Staging");
        assert!(list.assign_group("/tmp/a.db", Some("Production")));
        assert_eq!(list.entries()[0].group(), Some("Production"));
        assert!(list.assign_group("/tmp/a.db", Some("Staging")));
        assert_eq!(list.entries()[0].group(), Some("Staging"));
        // It appears in exactly one section, and it is the new one.
        let sections = list.arrange("");
        assert_eq!(sections[0].name.as_deref(), Some("Production"));
        assert!(sections[0].entries.is_empty());
        assert_eq!(sections[1].name.as_deref(), Some("Staging"));
        assert_eq!(sections[1].entries.len(), 1);
        assert_eq!(sections.len(), 2, "nothing is left ungrouped: {sections:?}");
        // Ungrouping puts it back in the ungrouped section.
        assert!(list.assign_group("/tmp/a.db", None));
        assert_eq!(list.entries()[0].group(), None);
        assert_eq!(list.arrange("").last().unwrap().name, None);
        // Re-applying the same assignment reports no change, so the caller
        // can skip a write to disk.
        assert!(!list.assign_group("/tmp/a.db", None));
    }

    #[test]
    fn assigning_an_unknown_group_or_locator_changes_nothing() {
        let mut list = list_of(&[saved("a", "/tmp/a.db")]);
        list.create_group("Production").unwrap();
        // A group that does not exist is refused rather than invented — the
        // only way to reach one is a stale UI, and a typo must not create a
        // group behind the user's back.
        assert!(!list.assign_group("/tmp/a.db", Some("Prodcution")));
        assert_eq!(list.entries()[0].group(), None);
        assert_eq!(list.groups(), ["Production"]);
        // An unknown connection assigns nothing.
        assert!(!list.assign_group("/tmp/missing.db", Some("Production")));
    }

    #[test]
    fn renaming_a_group_carries_its_members() {
        // Membership is stored by name, so a rename that touched only the
        // header would strand every member under a name nothing displays.
        let mut list = list_of(&[saved("a", "/tmp/a.db"), saved("b", "/tmp/b.db")]);
        list.create_group("Prod").unwrap();
        list.create_group("Other").unwrap();
        assert!(list.assign_group("/tmp/a.db", Some("Prod")));
        assert_eq!(
            list.rename_group("Prod", "  Production  ").unwrap(),
            "Production"
        );
        assert_eq!(list.groups(), ["Production", "Other"], "order is kept");
        assert_eq!(list.entries()[0].group(), Some("Production"));
        // The non-member is untouched.
        assert_eq!(list.entries()[1].group(), None);
        assert_eq!(list.arrange("")[0].entries.len(), 1);

        // A case-only change is a rename, not a collision with itself…
        assert_eq!(
            list.rename_group("Production", "PRODUCTION").unwrap(),
            "PRODUCTION"
        );
        assert_eq!(list.entries()[0].group(), Some("PRODUCTION"));
        // …but another group's name is refused however it is cased.
        assert_eq!(
            list.rename_group("PRODUCTION", "other"),
            Err(GroupError::Duplicate("Other".to_string()))
        );
        assert_eq!(
            list.rename_group("PRODUCTION", "   "),
            Err(GroupError::Empty)
        );
        assert_eq!(list.groups(), ["PRODUCTION", "Other"]);
    }

    #[test]
    fn an_empty_or_duplicate_group_name_is_refused() {
        let mut list = list_of(&[]);
        assert_eq!(list.create_group("  "), Err(GroupError::Empty));
        assert_eq!(list.create_group("  Prod  ").unwrap(), "Prod");
        assert_eq!(
            list.create_group("prod"),
            Err(GroupError::Duplicate("Prod".to_string())),
            "two groups differing only in case cannot both be made"
        );
        assert_eq!(list.groups(), ["Prod"]);
    }

    #[test]
    fn groups_reorder_one_step_at_a_time_and_stop_at_the_ends() {
        let mut list = list_of(&[]);
        for name in ["a", "b", "c"] {
            list.create_group(name).unwrap();
        }
        assert!(list.move_group("c", true));
        assert_eq!(list.groups(), ["a", "c", "b"]);
        assert!(list.move_group("c", true));
        assert_eq!(list.groups(), ["c", "a", "b"]);
        // At the ends the move is refused, which is what greys the button out.
        assert!(!list.move_group("c", true));
        assert_eq!(list.groups(), ["c", "a", "b"]);
        assert!(list.move_group("c", false));
        assert_eq!(list.groups(), ["a", "c", "b"]);
        assert!(!list.move_group("b", false));
        assert!(!list.move_group("nope", true));

        // The display order follows the group order, not the entry order.
        let mut list = list_of(&[saved("z", "/tmp/z.db")]);
        list.create_group("one").unwrap();
        list.create_group("two").unwrap();
        assert!(list.assign_group("/tmp/z.db", Some("two")));
        assert!(list.move_group("two", true));
        let names: Vec<Option<String>> = list.arrange("").into_iter().map(|s| s.name).collect();
        assert_eq!(
            names,
            vec![Some("two".to_string()), Some("one".to_string())]
        );
    }

    #[test]
    fn removing_a_group_ungroups_its_members_rather_than_removing_them() {
        // Deleting a folder must never delete the databases in it.
        let mut list = list_of(&[saved("a", "/tmp/a.db"), saved("b", "/tmp/b.db")]);
        list.create_group("Prod").unwrap();
        assert!(list.assign_group("/tmp/a.db", Some("Prod")));
        assert!(list.remove_group("Prod"));
        assert!(list.groups().is_empty());
        assert_eq!(list.entries().len(), 2, "both connections survive");
        assert_eq!(list.entries()[0].group(), None);
        assert!(
            !list.remove_group("Prod"),
            "removing it twice changes nothing"
        );
    }

    #[test]
    fn a_group_survives_an_edit_of_its_connection() {
        // The edit form collects no group, so it always supplies `None`.
        // Taking that literally would unfile a connection the moment its name
        // was changed — the FRE-111 failure, one field over.
        let mut list = list_of(&[saved_pg("prod", "postgres://u@h:5432/d")]);
        list.create_group("Production").unwrap();
        assert!(list.assign_group("postgres://u@h:5432/d", Some("Production")));
        for edited in [
            saved_pg("prod (renamed)", "postgres://u@h:5432/d"),
            saved_pg("prod", "postgres://u@h:5432/other"),
        ] {
            let mut list = list.clone();
            assert!(list.update("postgres://u@h:5432/d", edited));
            assert_eq!(list.entries()[0].group(), Some("Production"));
        }
    }

    #[test]
    fn a_group_survives_an_absorbed_collision_and_the_load_time_dedup() {
        // Both paths where two entries collapse into one. The survivor keeps
        // its own filing; the other's only fills a gap.
        let mut list = list_of(&[
            saved_pg("staging", "postgres://u@h:5432/staging"),
            saved_pg("prod", "postgres://u@h:5432/prod"),
        ]);
        list.create_group("Production").unwrap();
        assert!(list.assign_group("postgres://u@h:5432/prod", Some("Production")));
        assert!(list.update(
            "postgres://u@h:5432/staging",
            saved_pg("staging", "postgres://u@h:5432/prod"),
        ));
        assert_eq!(list.entries().len(), 1);
        assert_eq!(list.entries()[0].group(), Some("Production"));

        // The load-time dedup, which collapses two spellings of one server.
        let mut grouped = saved_pg("prod", "postgres://u@h:5432/db");
        grouped.set_group(Some("Production".into()));
        let plain = saved_pg("staging", "postgres://u@h/db");
        for entries in [
            vec![grouped.clone(), plain.clone()],
            vec![plain.clone(), grouped.clone()],
        ] {
            let deduped = normalize_and_dedup(entries);
            assert_eq!(deduped.len(), 1);
            assert_eq!(deduped[0].group(), Some("Production"));
        }
    }

    #[test]
    fn groups_stay_out_of_the_toml_until_one_exists() {
        // The unaffected-entries-serialize-unchanged half of the convention:
        // no groups means no `groups` key and no `group` key anywhere.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("connections.toml");
        let mut list = list_of(&[saved("plain", "/tmp/a.db")]);
        list.persist(&path).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(!text.contains("group"), "{text}");

        list.create_group("Production").unwrap();
        assert!(list.assign_group("/tmp/a.db", Some("Production")));
        list.persist(&path).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("groups = [\"Production\"]"), "{text}");
        assert!(text.contains("group = \"Production\""), "{text}");
        // And it all comes back.
        let (reloaded, err) = SavedList::load(&path);
        assert!(err.is_none());
        assert_eq!(reloaded.groups(), ["Production"]);
        assert_eq!(reloaded.entries()[0].group(), Some("Production"));
    }

    #[test]
    fn a_pre_fre_120_config_file_loads_ungrouped() {
        // The back-compat contract: a file written before FRE-120 has neither
        // key, and must load as an ungrouped list rather than failing.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("connections.toml");
        std::fs::write(
            &path,
            "[[connections]]\nkind = \"sqlite\"\nname = \"old\"\npath = \"/tmp/old.db\"\n\
             \n[[connections]]\nkind = \"postgres\"\nname = \"pg\"\nurl = \"postgres://u@h:5432/d\"\n",
        )
        .unwrap();
        let (list, err) = SavedList::load(&path);
        assert!(err.is_none());
        assert!(list.groups().is_empty());
        for entry in list.entries() {
            assert_eq!(entry.group(), None);
        }
        // One ungrouped section, so the list renders exactly as it did.
        let sections = list.arrange("");
        assert_eq!(sections.len(), 1);
        assert_eq!(sections[0].name, None);
        assert_eq!(sections[0].entries.len(), 2);
    }

    #[test]
    fn a_member_of_an_undeclared_group_adopts_it_at_load() {
        // Hand-edited files: the list is drawn from `groups`, so a group only
        // an entry names would render its members nowhere. An empty name is
        // not a group at all.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("connections.toml");
        std::fs::write(
            &path,
            "groups = [\"Declared\", \"  \"]\n\
             \n[[connections]]\nkind = \"sqlite\"\nname = \"a\"\npath = \"/tmp/a.db\"\ngroup = \"Invented\"\n\
             \n[[connections]]\nkind = \"sqlite\"\nname = \"b\"\npath = \"/tmp/b.db\"\ngroup = \"  \"\n",
        )
        .unwrap();
        let (list, err) = SavedList::load(&path);
        assert!(err.is_none());
        assert_eq!(list.groups(), ["Declared", "Invented"]);
        assert_eq!(list.entries()[0].group(), Some("Invented"));
        assert_eq!(list.entries()[1].group(), None, "whitespace is not a group");
        // Every entry is reachable from a section.
        let listed: usize = list.arrange("").iter().map(|s| s.entries.len()).sum();
        assert_eq!(listed, 2);
    }

    #[test]
    fn search_matches_connection_names_case_insensitively() {
        assert!(name_matches("Production DB", "prod"));
        assert!(name_matches("production db", "DB"));
        assert!(name_matches("Årsrapport", "årsrapp"));
        assert!(!name_matches("Production", "staging"));
        // An empty (or whitespace-only) query matches everything, so the
        // unfiltered list renders through the same path.
        assert!(name_matches("anything", ""));
        assert!(name_matches("anything", "   "));
        // The name alone — not the URL, whatever the box sits next to.
        assert!(!name_matches("prod", "db.example.com"));
    }

    #[test]
    fn a_search_narrows_the_sections_and_drops_the_ones_with_no_hit() {
        let mut list = list_of(&[
            saved("music.db", "/tmp/music.db"),
            saved_pg("prod orders", "postgres://u@h:5432/orders"),
            saved_pg("prod billing", "postgres://u@h:5432/billing"),
        ]);
        list.create_group("Production").unwrap();
        list.create_group("Empty").unwrap();
        assert!(list.assign_group("postgres://u@h:5432/orders", Some("Production")));
        assert!(list.assign_group("postgres://u@h:5432/billing", Some("Production")));

        // Unfiltered: both groups show, the empty one included — it is a thing
        // the user made, and a group that hid until it had a member could
        // never be filled.
        let all = list.arrange("");
        assert_eq!(all.len(), 3);
        assert_eq!(all[1].name.as_deref(), Some("Empty"));
        assert!(all[1].entries.is_empty());

        // Searching: only sections with a hit, and only the hits.
        let hits = list.arrange("BILL");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].name.as_deref(), Some("Production"));
        assert_eq!(hits[0].entries.len(), 1);
        assert_eq!(hits[0].entries[0].name(), "prod billing");
        // A hit in the ungrouped section alone leaves only that section.
        let hits = list.arrange("music");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].name, None);
        // Nothing at all is an empty arrangement, not a page of headers.
        assert!(list.arrange("zzz").is_empty());
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
    fn marking_stays_absent_from_the_toml_until_it_is_set() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("connections.toml");
        // An unmarked entry must serialize byte-for-byte as it did before
        // FRE-111: no protection key, no color key.
        save_connections(&path, &[saved("plain", "/tmp/a.db")]).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(!text.contains("protection"), "{text}");
        assert!(!text.contains("color"), "{text}");

        let mut marked = saved("prod", "/tmp/b.db");
        marked.set_marking(WriteProtection::Confirm, Some(ConnectionColor::Red));
        save_connections(&path, &[marked.clone()]).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("protection = \"confirm\""), "{text}");
        assert!(text.contains("color = \"red\""), "{text}");
        assert_eq!(load_connections(&path).unwrap(), vec![marked]);
    }

    #[test]
    fn a_pre_fre_111_config_file_loads_as_unprotected() {
        // The back-compat contract: a config file written before FRE-111 has
        // neither key, and must come back as Open with no colour rather than
        // failing to parse.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("connections.toml");
        std::fs::write(
            &path,
            "[[connections]]\nkind = \"sqlite\"\nname = \"old\"\npath = \"/tmp/old.db\"\n\
             \n[[connections]]\nkind = \"postgres\"\nname = \"pg\"\nurl = \"postgres://u@h:5432/d\"\n",
        )
        .unwrap();
        let loaded = load_connections(&path).unwrap();
        assert_eq!(loaded.len(), 2);
        for entry in &loaded {
            assert_eq!(entry.protection(), WriteProtection::Open);
            assert_eq!(entry.color(), None);
        }
    }

    #[test]
    fn marking_round_trips_on_every_backend() {
        for mut entry in [
            saved("s", "/tmp/s.db"),
            saved_pg("p", "postgres://u@h:5432/d"),
            saved_ms("m", "mssql://sa@h:1433/d"),
        ] {
            entry.set_marking(WriteProtection::ReadOnly, Some(ConnectionColor::Purple));
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("connections.toml");
            save_connections(&path, std::slice::from_ref(&entry)).unwrap();
            assert_eq!(load_connections(&path).unwrap(), vec![entry]);
        }
    }

    #[test]
    fn set_marking_finds_the_entry_and_reports_whether_anything_changed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("connections.toml");
        save_connections(
            &path,
            &[
                saved("a", "/tmp/a.db"),
                saved_pg("b", "postgres://u@h:5432/d"),
            ],
        )
        .unwrap();
        let (mut list, err) = SavedList::load(&path);
        assert!(err.is_none());
        assert!(list.set_marking("/tmp/a.db", WriteProtection::Confirm, None));
        assert_eq!(list.entries()[0].protection(), WriteProtection::Confirm);
        // The other entry is untouched.
        assert_eq!(list.entries()[1].protection(), WriteProtection::Open);
        // Re-applying the same marking reports no change, so the caller can
        // skip a pointless write to disk.
        assert!(!list.set_marking("/tmp/a.db", WriteProtection::Confirm, None));
        assert!(list.set_marking(
            "/tmp/a.db",
            WriteProtection::Confirm,
            Some(ConnectionColor::Amber)
        ));
        assert_eq!(list.entries()[0].color(), Some(ConnectionColor::Amber));
        // An unknown locator marks nothing.
        assert!(!list.set_marking("/tmp/missing.db", WriteProtection::ReadOnly, None));
    }

    #[test]
    fn editing_a_connection_keeps_its_marking() {
        // The edit form collects no marking, so it always supplies defaults.
        // Taking those literally would unprotect a connection the moment its
        // name was changed — and the loss would only surface at next launch.
        let mut list = list_of(&[saved_pg("prod", "postgres://u@h:5432/d")]);
        assert!(list.set_marking(
            "postgres://u@h:5432/d",
            WriteProtection::ReadOnly,
            Some(ConnectionColor::Red)
        ));

        // An in-place rename, and an edit that moves the locator.
        for edited in [
            saved_pg("prod (renamed)", "postgres://u@h:5432/d"),
            saved_pg("prod", "postgres://u@h:5432/other"),
        ] {
            let mut list = list.clone();
            assert!(list.update("postgres://u@h:5432/d", edited));
            assert_eq!(list.entries()[0].protection(), WriteProtection::ReadOnly);
            assert_eq!(list.entries()[0].color(), Some(ConnectionColor::Red));
        }
    }

    #[test]
    fn an_edit_that_absorbs_another_entry_takes_the_stricter_marking() {
        // Repointing unprotected `staging` at prod's URL absorbs the marked
        // entry. Keeping the editee's marking would leave an Open entry
        // addressing the read-only database — protection gone with the row.
        let mut list = list_of(&[
            saved_pg("staging", "postgres://u@h:5432/staging"),
            saved_pg("prod", "postgres://u@h:5432/prod"),
        ]);
        assert!(list.set_marking(
            "postgres://u@h:5432/prod",
            WriteProtection::ReadOnly,
            Some(ConnectionColor::Red)
        ));

        assert!(list.update(
            "postgres://u@h:5432/staging",
            saved_pg("staging", "postgres://u@h:5432/prod"),
        ));
        assert_eq!(list.entries().len(), 1, "the collision was absorbed");
        assert_eq!(
            list.entries()[0].protection(),
            WriteProtection::ReadOnly,
            "the stricter marking survives the absorption"
        );
        assert_eq!(list.entries()[0].color(), Some(ConnectionColor::Red));
    }

    #[test]
    fn a_marking_survives_the_load_time_dedup() {
        // Reachable without hand-editing the file: `update` compares the
        // stored locator string, this dedup compares *normalized* URLs. Mark
        // prod read-only, then point staging at the same server spelled
        // without the port — no string collision, so both persist. They only
        // collapse at the next launch, and discarding the loser's marking
        // there would leave an unmarked entry addressing production.
        let mut marked = saved_pg("prod", "postgres://u@h:5432/db");
        marked.set_marking(WriteProtection::ReadOnly, Some(ConnectionColor::Red));
        let plain = saved_pg("staging", "postgres://u@h/db");

        // Whichever order they sit in, the marking survives the collapse.
        for entries in [
            vec![marked.clone(), plain.clone()],
            vec![plain.clone(), marked.clone()],
        ] {
            let deduped = normalize_and_dedup(entries);
            assert_eq!(deduped.len(), 1, "the two spellings are one server");
            assert_eq!(
                deduped[0].protection(),
                WriteProtection::ReadOnly,
                "protection must not vanish between one run and the next"
            );
            assert_eq!(deduped[0].color(), Some(ConnectionColor::Red));
        }
    }

    #[test]
    fn protection_orders_least_to_most_protective() {
        // `update` merges markings with `max`, so this order is load-bearing.
        assert!(WriteProtection::Open < WriteProtection::Confirm);
        assert!(WriteProtection::Confirm < WriteProtection::ReadOnly);
        assert_eq!(
            WriteProtection::Open.max(WriteProtection::ReadOnly),
            WriteProtection::ReadOnly
        );
    }

    /// A `SavedList` over `entries`, via a real load so `load_failed` is false
    /// and `persist`/`set_marking` behave as they do in the app.
    fn list_of(entries: &[SavedConnection]) -> SavedList {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("connections.toml");
        save_connections(&path, entries).unwrap();
        let (list, err) = SavedList::load(&path);
        assert!(err.is_none());
        list
    }

    #[test]
    fn marking_never_disturbs_the_rest_of_the_entry() {
        // Marking is not an edit: the locator, name, tunnel and auth must all
        // survive it untouched, or a marked connection would stop connecting.
        let original = SavedConnection::Postgres {
            name: "prod".into(),
            url: "postgres://u@h:5432/d".into(),
            tunnel: Some(tunnel(crate::tunnel::TunnelAuth::Agent)),
            auth: ServerAuth::Entra(EntraAuth::interactive_default()),
            protection: WriteProtection::Open,
            color: None,
            group: None,
        };
        let mut marked = original.clone();
        marked.set_marking(WriteProtection::ReadOnly, Some(ConnectionColor::Red));
        assert_eq!(marked.locator(), original.locator());
        assert_eq!(marked.name(), original.name());
        assert_eq!(marked.backend(), original.backend());
        let (
            SavedConnection::Postgres {
                tunnel: a, auth: b, ..
            },
            SavedConnection::Postgres {
                tunnel: c, auth: d, ..
            },
        ) = (&marked, &original)
        else {
            unreachable!("both are Postgres entries")
        };
        assert_eq!((a, b), (c, d));
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
            auth: ServerAuth::Entra(EntraAuth::Interactive {
                tenant: "contoso.onmicrosoft.com".into(),
                client_id: None,
            }),
            protection: WriteProtection::Open,
            color: None,
            group: None,
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
            auth: ServerAuth::Entra(EntraAuth::interactive_default()),
            protection: WriteProtection::Open,
            color: None,
            group: None,
        };
        assert!(list.add(updated.clone()));
        assert_eq!(list.entries().len(), 1);
        match &list.entries()[0] {
            SavedConnection::SqlServer {
                name, tunnel, auth, ..
            } => {
                assert_eq!(name, "prod");
                assert!(tunnel.is_some());
                assert!(matches!(auth, ServerAuth::Entra(_)));
            }
            other => panic!("expected sqlserver, got {other:?}"),
        }
        // Identical settings again: no change.
        assert!(!list.add(updated));
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
                auth: ServerAuth::Password,
                protection: WriteProtection::Open,
                color: None,
                group: None,
            },
            SavedConnection::Postgres {
                name: "via key".into(),
                url: "postgres://u@db2.internal:5432/app".into(),
                tunnel: Some(tunnel(crate::tunnel::TunnelAuth::KeyFile {
                    path: PathBuf::from("/home/u/.ssh/id_ed25519"),
                })),
                auth: ServerAuth::Password,
                protection: WriteProtection::Open,
                color: None,
                group: None,
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
    fn server_auth_serialized_form_is_pinned_across_the_rename() {
        // `ServerAuth` was `PgAuth` until FRE-144. It serializes by contents
        // (the `kind` tag), so the Rust name never reaches the TOML — this
        // pins that shape so a future rename or attribute change can't move
        // the on-disk format.
        let password = toml::to_string(&ServerAuth::Password).unwrap();
        assert_eq!(password, "kind = \"password\"\n");
        let entra = toml::to_string(&ServerAuth::Entra(EntraAuth::Interactive {
            tenant: "contoso.onmicrosoft.com".into(),
            client_id: None,
        }))
        .unwrap();
        assert!(entra.contains("kind = \"entra\""), "{entra}");
        assert_eq!(
            toml::from_str::<ServerAuth>(&entra).unwrap(),
            ServerAuth::Entra(EntraAuth::Interactive {
                tenant: "contoso.onmicrosoft.com".into(),
                client_id: None,
            })
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
                auth: ServerAuth::Entra(EntraAuth::Interactive {
                    tenant: "contoso.onmicrosoft.com".into(),
                    client_id: None,
                }),
                protection: WriteProtection::Open,
                color: None,
                group: None,
            },
            SavedConnection::Postgres {
                name: "azure-mi".into(),
                url: "postgres://mi@other.postgres.database.azure.com:5432/db".into(),
                tunnel: None,
                auth: ServerAuth::Entra(EntraAuth::ManagedIdentity {
                    client_id: Some("11111111-2222-3333-4444-555555555555".into()),
                }),
                protection: WriteProtection::Open,
                color: None,
                group: None,
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
            auth: ServerAuth::Entra(EntraAuth::interactive_default()),
            protection: WriteProtection::Open,
            color: None,
            group: None,
        };
        assert!(list.add(entra));
        assert_eq!(list.entries().len(), 1);
        match &list.entries()[0] {
            SavedConnection::Postgres { name, auth, .. } => {
                assert_eq!(name, "prod");
                assert!(matches!(auth, ServerAuth::Entra(_)));
            }
            other => panic!("expected postgres, got {other:?}"),
        }
        // Re-adding the identical entry is a no-op.
        let same = SavedConnection::Postgres {
            name: "prod".into(),
            url: "postgres://u@h:5432/db".into(),
            tunnel: None,
            auth: ServerAuth::Entra(EntraAuth::interactive_default()),
            protection: WriteProtection::Open,
            color: None,
            group: None,
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
            auth: ServerAuth::Password,
            protection: WriteProtection::Open,
            color: None,
            group: None,
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
    fn update_replaces_the_entry_in_place_including_its_name() {
        let dir = tempfile::tempdir().unwrap();
        let (mut list, _) = SavedList::load(&dir.path().join("connections.toml"));
        list.add(saved_pg("first", "postgres://u@a:5432/db"));
        list.add(saved_pg("prod", "postgres://u@h:5432/db"));
        list.add(saved_pg("last", "postgres://u@z:5432/db"));

        // A rename with no locator change: `add` would have ignored this.
        assert!(list.update(
            "postgres://u@h:5432/db",
            saved_pg("renamed", "postgres://u@h:5432/db")
        ));
        assert_eq!(list.entries()[1].name(), "renamed");
        // Position is preserved, so the list doesn't reshuffle under the user.
        assert_eq!(list.entries().len(), 3);
        assert_eq!(list.entries()[0].name(), "first");
        assert_eq!(list.entries()[2].name(), "last");
    }

    #[test]
    fn update_moves_the_locator_and_absorbs_a_collision() {
        let dir = tempfile::tempdir().unwrap();
        let (mut list, _) = SavedList::load(&dir.path().join("connections.toml"));
        list.add(saved_pg("prod", "postgres://u@h:5432/db"));
        list.add(saved_pg("staging", "postgres://u@s:5432/db"));

        // Editing "prod" onto "staging"'s locator leaves one entry, not two
        // sharing a key.
        assert!(list.update(
            "postgres://u@h:5432/db",
            saved_pg("merged", "postgres://u@s:5432/db")
        ));
        assert_eq!(list.entries().len(), 1);
        assert_eq!(list.entries()[0].name(), "merged");
        assert_eq!(list.entries()[0].locator(), "postgres://u@s:5432/db");
    }

    #[test]
    fn update_of_a_missing_entry_changes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let (mut list, _) = SavedList::load(&dir.path().join("connections.toml"));
        list.add(saved_pg("prod", "postgres://u@h:5432/db"));
        assert!(!list.update(
            "postgres://u@gone:5432/db",
            saved_pg("x", "postgres://u@x:5432/db")
        ));
        assert_eq!(list.entries().len(), 1);
        assert_eq!(list.entries()[0].name(), "prod");
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

        assert!(list.remove("/tmp/missing.db").is_none());
        assert!(list.remove("/tmp/a.db").is_some());
        assert!(list.entries().is_empty());
    }

    #[test]
    fn edit_prefill_splits_the_url_into_the_forms_fields() {
        // Postgres reads back its own option key; the user is percent-decoded
        // to what was typed into the field.
        let prefill = EditPrefill::from_saved(saved_pg(
            "prod",
            "postgres://a%40b.com@db.example.com:5432/app?sslmode=require",
        ));
        assert_eq!(prefill.name, "prod");
        assert_eq!(prefill.host, "db.example.com");
        assert_eq!(prefill.port, "5432");
        assert_eq!(prefill.database, "app");
        assert_eq!(prefill.user, "a@b.com");
        assert_eq!(prefill.option.as_deref(), Some("require"));
        // sslmode is not trustServerCertificate; the flag stays off.
        assert!(!prefill.trust_cert);
        assert_eq!(prefill.auth_mode, "password");

        // SQL Server owns `encrypt`, and reads the trust flag whatever its
        // casing — a hand-pasted URL may spell it any way.
        let prefill = EditPrefill::from_saved(saved_ms(
            "ms",
            "mssql://sa@db.example.com:1433/app?encrypt=off&TRUSTSERVERCERTIFICATE=yes",
        ));
        assert_eq!(prefill.option.as_deref(), Some("off"));
        assert!(prefill.trust_cert);

        // An unparseable URL leaves the fields empty rather than failing: the
        // form still opens, showing the name it does know.
        let prefill = EditPrefill::from_saved(saved_pg("broken", "not a url"));
        assert_eq!(prefill.name, "broken");
        assert_eq!(prefill.host, "");
        assert_eq!(prefill.port, "");
    }

    #[test]
    fn edit_prefill_carries_auth_and_tunnel_settings() {
        let entra = SavedConnection::Postgres {
            name: "az".into(),
            url: "postgres://you@srv.postgres.database.azure.com:5432/app".into(),
            tunnel: Some(TunnelConfig {
                host: "bastion".into(),
                port: 2222,
                user: "ops".into(),
                auth: crate::tunnel::TunnelAuth::KeyFile {
                    path: PathBuf::from("/home/me/.ssh/id_ed25519"),
                },
            }),
            auth: ServerAuth::Entra(EntraAuth::Interactive {
                tenant: "contoso.com".into(),
                client_id: Some("abc-123".into()),
            }),
            protection: WriteProtection::Open,
            color: None,
            group: None,
        };
        let prefill = EditPrefill::from_saved(entra);
        assert_eq!(prefill.auth_mode, "entra-interactive");
        assert_eq!(prefill.entra_tenant, "contoso.com");
        assert_eq!(prefill.entra_client_id, "abc-123");
        assert_eq!(prefill.ssh_host, "bastion");
        assert_eq!(prefill.ssh_port, "2222");
        assert_eq!(prefill.ssh_user, "ops");
        assert!(prefill.ssh_use_key);
        assert_eq!(prefill.ssh_key_path, "/home/me/.ssh/id_ed25519");

        // Managed identity has no tenant of its own, so the form's default
        // stands; an agent tunnel leaves the key-file fields empty.
        let mi = SavedConnection::SqlServer {
            name: "mi".into(),
            url: "mssql://you@srv.database.windows.net:1433/app".into(),
            tunnel: Some(TunnelConfig {
                host: "bastion".into(),
                port: 22,
                user: "ops".into(),
                auth: crate::tunnel::TunnelAuth::Agent,
            }),
            auth: ServerAuth::Entra(EntraAuth::ManagedIdentity { client_id: None }),
            protection: WriteProtection::Open,
            color: None,
            group: None,
        };
        let prefill = EditPrefill::from_saved(mi);
        assert_eq!(prefill.auth_mode, "entra-mi");
        assert_eq!(prefill.entra_tenant, "organizations");
        assert_eq!(prefill.entra_client_id, "");
        assert!(!prefill.ssh_use_key);
        assert_eq!(prefill.ssh_key_path, "");
    }

    #[test]
    fn a_sqlite_entry_prefills_only_its_name() {
        // SQLite has no edit form; the conversion still has to be total.
        let prefill = EditPrefill::from_saved(saved("local", "/tmp/a.db"));
        assert_eq!(prefill.name, "local");
        assert_eq!(
            prefill,
            EditPrefill {
                name: "local".into(),
                ..EditPrefill::default()
            }
        );
    }
}
