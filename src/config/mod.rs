//! Persistence for the app's config files (XDG config dir, TOML), split by
//! persistence domain: the saved-connections list ([`connections`]), user
//! preferences ([`settings`]) and the last session ([`session`]).
//!
//! The three domains deliberately carry different error policies —
//! connections are strict (a malformed file is an error, never silent data
//! loss), settings and session are lenient (a bad file yields defaults) —
//! each policy's rationale lives with its loader. Everything is re-exported
//! here, so callers keep addressing `config::…` regardless of which file a
//! type lives in.

mod connections;
mod session;
mod settings;

pub use connections::*;
pub use session::*;
pub use settings::*;

use std::fmt;
use std::path::{Path, PathBuf};

use serde::de::DeserializeOwned;
use serde::Serialize;

/// The directory hubro's files live in, under both the platform config dir
/// (connections, settings, session, SSH known_hosts) and the platform data dir
/// (query history).
///
/// Load-bearing: renaming it strands every user's existing files, and there
/// is deliberately no migration code. Spelled once so the five paths built on
/// it cannot drift apart. The keyring service and the bundle id carry the same
/// word but are separate namespaces, each with its own constant.
const APP_DIR: &str = "hubro";

/// `<config dir>/hubro/<name>` — `$XDG_CONFIG_HOME/hubro/<name>` on Linux.
/// `None` when the platform reports no config directory.
pub fn app_config_file(name: &str) -> Option<PathBuf> {
    Some(dirs::config_dir()?.join(APP_DIR).join(name))
}

/// `<data dir>/hubro/<name>` — `$XDG_DATA_HOME/hubro/<name>` on Linux.
/// `None` when the platform reports no data directory.
pub fn app_data_file(name: &str) -> Option<PathBuf> {
    Some(dirs::data_dir()?.join(APP_DIR).join(name))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigError(pub String);

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "config error: {}", self.0)
    }
}

impl std::error::Error for ConfigError {}

/// Serializes `value` to TOML and writes it to `path`, creating parent
/// directories as needed. Writes to a temp file and renames so a crash
/// mid-write can't corrupt the file — the one write path shared by all three
/// domains' `save_*` functions.
pub(crate) fn write_toml_atomic<T: Serialize>(path: &Path, value: &T) -> Result<(), ConfigError> {
    let text = toml::to_string_pretty(value).map_err(|err| ConfigError(err.to_string()))?;
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

/// Loads a file under the **lenient** error policy: a missing, unreadable or
/// malformed file yields `T::default()`, never an error. The read half of the
/// settings and session domains; what a caller may then *write* back over a
/// file this returned defaults for is its own business (see
/// `settings::set_settings_key`, which must not).
pub(crate) fn load_lenient<T: DeserializeOwned + Default>(path: &Path) -> T {
    match std::fs::read_to_string(path) {
        Ok(text) => toml::from_str(&text).unwrap_or_default(),
        Err(_) => T::default(),
    }
}

#[cfg(test)]
mod tests {
    /// Every file hubro keeps outside a user's databases, resolved, against
    /// the paths spelled out in full. The directory name is load-bearing —
    /// changing it strands every existing install's files — so this pins the
    /// *result* rather than the constant: routing a path through the wrong
    /// helper (config dir for data) fails here too.
    #[test]
    fn app_files_resolve_to_the_paths_existing_installs_use() {
        let config = dirs::config_dir().expect("a config dir").join("hubro");
        let data = dirs::data_dir().expect("a data dir").join("hubro");
        let resolved = [
            (
                super::default_config_path(),
                config.join("connections.toml"),
            ),
            (super::default_settings_path(), config.join("settings.toml")),
            (super::default_session_path(), config.join("session.toml")),
            (
                crate::tunnel::app_known_hosts_path(),
                config.join("known_hosts"),
            ),
            (
                crate::history::default_history_path(),
                data.join("history.db"),
            ),
        ];
        for (actual, expected) in resolved {
            assert_eq!(actual, Some(expected));
        }
    }
}
