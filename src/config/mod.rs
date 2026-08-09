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
use std::path::Path;

use serde::Serialize;

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
