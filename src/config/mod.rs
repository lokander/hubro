//! Persistence for the app's config files (XDG config dir, TOML), split by
//! persistence domain: the saved-connections list ([`connections`]), user
//! preferences ([`settings`]) and the last session ([`session`]).
//!
//! The three domains deliberately carry different error policies —
//! connections are strict (a malformed file is an error, never silent data
//! loss), settings and session are lenient (a bad file yields defaults;
//! for settings, a bad *value* costs only its own setting, and what was not
//! used is reported — see `settings::interpret_settings`) — each policy's
//! rationale lives with its loader. Everything is re-exported
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
/// is deliberately no migration code. Spelled once, in `AppPaths::resolve`,
/// so the five paths built on it cannot drift apart. The keyring service and
/// the bundle id carry the same word but are separate namespaces, each with
/// its own constant.
const APP_DIR: &str = "hubro";

/// Every file hubro keeps outside a user's databases, resolved once.
///
/// The app builds one from the platform's directories ([`Self::platform`]);
/// a test builds one under a directory it owns ([`Self::under`]). Nothing
/// else resolves these paths: every read and write of them goes through the
/// `AppPaths` its `AppState` was built with, so a test's state cannot reach
/// the user's real files by any route a component or task happens to take.
/// A `None` is a directory the platform did not report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppPaths {
    /// The saved-connections list: `<config dir>/hubro/connections.toml`.
    pub connections: Option<PathBuf>,
    /// User preferences and window geometry: `<config dir>/hubro/settings.toml`.
    pub settings: Option<PathBuf>,
    /// The last session's tabs: `<config dir>/hubro/session.toml`. Kept
    /// separate from `settings.toml` because it is transient, churns often,
    /// and is fine to lose — whereas a corrupt settings file must not take
    /// user preferences with it.
    pub session: Option<PathBuf>,
    /// hubro's own SSH known_hosts store, which trust decisions are written
    /// to: `<config dir>/hubro/known_hosts`.
    pub known_hosts: Option<PathBuf>,
    /// The user's OpenSSH `~/.ssh/known_hosts`: read, never written.
    pub ssh_known_hosts: Option<PathBuf>,
    /// Query history: `<data dir>/hubro/history.db`.
    pub history: Option<PathBuf>,
}

impl AppPaths {
    /// The real files, under the platform's config, data and home
    /// directories.
    ///
    /// **Panics in this crate's unit tests.** A test that reached it would
    /// read, and on any save write, the developer's own connections,
    /// settings, session, history and known_hosts — one engine test did
    /// exactly that, adding a connection to the real list on every run. So a
    /// test builds its state on [`Self::under`] a temp dir, and this guard
    /// turns any route to the real files it missed into a failing test
    /// rather than a silent write. `tests/` links the library without
    /// `cfg(test)`, so `no_integration_test_names_the_real_paths` covers it.
    pub fn platform() -> Self {
        if cfg!(test) {
            panic!(
                "a test resolved hubro's real config/data paths; build its \
                 state on AppPaths::under(<temp dir>) instead"
            );
        }
        Self::from_platform_dirs()
    }

    /// The real resolution, unguarded. Private, so only [`Self::platform`]
    /// and the test pinning the paths existing installs use can call it; it
    /// resolves, and touches no file.
    fn from_platform_dirs() -> Self {
        Self::resolve(dirs::config_dir(), dirs::data_dir(), dirs::home_dir())
    }

    /// Every file inside `dir`, laid out as on a real install (`config/`,
    /// `data/`, `home/`). Nothing is created until something is saved.
    pub fn under(dir: &Path) -> Self {
        Self::resolve(
            Some(dir.join("config")),
            Some(dir.join("data")),
            Some(dir.join("home")),
        )
    }

    fn resolve(config: Option<PathBuf>, data: Option<PathBuf>, home: Option<PathBuf>) -> Self {
        let config_file = |name: &str| config.as_ref().map(|dir| dir.join(APP_DIR).join(name));
        Self {
            connections: config_file("connections.toml"),
            settings: config_file("settings.toml"),
            session: config_file("session.toml"),
            known_hosts: config_file("known_hosts"),
            ssh_known_hosts: home.map(|home| home.join(".ssh").join("known_hosts")),
            history: data.map(|dir| dir.join(APP_DIR).join("history.db")),
        }
    }

    /// The files consulted to decide whether a server host key is trusted:
    /// hubro's own store first, then the user's OpenSSH one (so hosts
    /// already trusted via `ssh` connect without a prompt). Passed into
    /// [`crate::tunnel::Tunnel::open`].
    pub fn known_hosts_read(&self) -> Vec<PathBuf> {
        [&self.known_hosts, &self.ssh_known_hosts]
            .into_iter()
            .flatten()
            .cloned()
            .collect()
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

/// Serializes `value` to TOML and writes it to `path`, creating parent
/// directories as needed. Writes to a temp file and renames so a crash
/// mid-write can't corrupt the file. Every whole-file save goes through it;
/// settings, which are edited key by key, go through [`write_text_atomic`].
pub(crate) fn write_toml_atomic<T: Serialize>(path: &Path, value: &T) -> Result<(), ConfigError> {
    let text = toml::to_string_pretty(value).map_err(|err| ConfigError(err.to_string()))?;
    write_text_atomic(path, &text)
}

/// Writes already-rendered `text` to `path` the way [`write_toml_atomic`]
/// does — parent directories created, temp file then rename.
pub(crate) fn write_text_atomic(path: &Path, text: &str) -> Result<(), ConfigError> {
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
/// session domain; settings have their own loader, which falls back field by
/// field (`settings::interpret_settings`). What a caller may then *write*
/// back over a file this returned defaults for is its own business.
pub(crate) fn load_lenient<T: DeserializeOwned + Default>(path: &Path) -> T {
    match std::fs::read_to_string(path) {
        Ok(text) => toml::from_str(&text).unwrap_or_default(),
        Err(_) => T::default(),
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::path::{Path, PathBuf};

    use super::AppPaths;

    /// Every file hubro keeps outside a user's databases, resolved, against
    /// the paths spelled out in full. The directory name is load-bearing —
    /// changing it strands every existing install's files — so this pins the
    /// *result* rather than the constant: routing a path through the wrong
    /// directory (config for data) fails here too. Resolving touches no file,
    /// which is why this one test may call the unguarded resolver.
    #[test]
    fn app_files_resolve_to_the_paths_existing_installs_use() {
        let config = dirs::config_dir().expect("a config dir").join("hubro");
        let data = dirs::data_dir().expect("a data dir").join("hubro");
        let home = dirs::home_dir().expect("a home dir");
        let paths = AppPaths::from_platform_dirs();
        assert_eq!(
            paths,
            AppPaths {
                connections: Some(config.join("connections.toml")),
                settings: Some(config.join("settings.toml")),
                session: Some(config.join("session.toml")),
                known_hosts: Some(config.join("known_hosts")),
                ssh_known_hosts: Some(home.join(".ssh").join("known_hosts")),
                history: Some(data.join("history.db")),
            }
        );
        // hubro's own store first, then the user's.
        assert_eq!(
            paths.known_hosts_read(),
            [
                config.join("known_hosts"),
                home.join(".ssh").join("known_hosts")
            ]
        );
    }

    /// The guard: a unit test that reaches the real paths — through
    /// `AppState::new`, the `App` component, or anything added later — fails
    /// here instead of reading and writing the developer's own files.
    #[test]
    #[should_panic(expected = "a test resolved hubro's real config/data paths")]
    fn a_unit_test_cannot_resolve_the_real_paths() {
        AppPaths::platform();
    }

    /// What a test's state is built on keeps *every* file inside the dir it
    /// was given. Destructured without `..`, so a file added to `AppPaths`
    /// does not compile here until it is checked too.
    #[test]
    fn under_keeps_every_file_inside_its_dir() {
        let dir = Path::new("/scratch/hubro-test");
        let AppPaths {
            connections,
            settings,
            session,
            known_hosts,
            ssh_known_hosts,
            history,
        } = AppPaths::under(dir);
        for path in [
            connections,
            settings,
            session,
            known_hosts,
            ssh_known_hosts,
            history,
        ] {
            let path = path.expect("under() resolves every path");
            assert!(path.starts_with(dir), "{} escapes the dir", path.display());
        }
    }

    /// Every `.rs` file under `dir`, recursively, with its comment lines
    /// blanked — so prose naming a function cannot trip, or satisfy, a scan.
    fn sources(dir: &Path) -> Vec<(PathBuf, String)> {
        let mut found = Vec::new();
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                found.extend(sources(&path));
            } else if path.extension().is_some_and(|ext| ext == "rs") {
                let code = std::fs::read_to_string(&path)
                    .unwrap()
                    .lines()
                    .map(|line| {
                        // Blanked rather than dropped, so line numbers hold.
                        if line.trim_start().starts_with("//") {
                            ""
                        } else {
                            line
                        }
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                found.push((path, code));
            }
        }
        found
    }

    /// Every way code computes a user's real directory: the `dirs` crate
    /// (named however it is imported), std's `home_dir`, and the environment
    /// variables the platforms keep those directories in. A word ending in
    /// `*` matches as a prefix.
    pub(crate) const PLATFORM_DIR_WORDS: &[&str] = &[
        "dirs",
        "home_dir",
        "HOME",
        "USERPROFILE",
        "APPDATA",
        "LOCALAPPDATA",
        "XDG_*",
    ];

    /// Every line of a `.rs` file under `dir` that names one of `words` as a
    /// whole identifier, as `path:line: text`, except lines whose trimmed
    /// text is exactly one of `allowed`. Comment lines are blanked; string
    /// literals are **not** skipped — telling them apart takes a lexer, and a
    /// hand-rolled one is fooled by `'"'` — so a literal that must name a
    /// word is allowed by its exact line instead. Panics if `dir` holds
    /// suspiciously few files, so a wrong path cannot pass by scanning
    /// nothing.
    pub(crate) fn words_named_in(dir: &Path, words: &[&str], allowed: &[&str]) -> Vec<String> {
        let sources = sources(dir);
        assert!(
            sources.len() > 20,
            "{} was not scanned: {}",
            dir.display(),
            sources.len()
        );
        let matches = |word: &str| {
            words.iter().any(|pattern| match pattern.strip_suffix('*') {
                Some(prefix) => word.starts_with(prefix),
                None => word == *pattern,
            })
        };
        let mut found = Vec::new();
        for (path, code) in &sources {
            for (number, line) in code.lines().enumerate() {
                if allowed.contains(&line.trim()) {
                    continue;
                }
                if line
                    .split(|c: char| !(c.is_alphanumeric() || c == '_'))
                    .any(matches)
                {
                    found.push(format!(
                        "{}:{}: {}",
                        path.display(),
                        number + 1,
                        line.trim()
                    ));
                }
            }
        }
        found
    }

    /// The guard holds only while [`AppPaths::from_platform_dirs`] is the
    /// one place in `src/` that computes a user's real directory: a
    /// `dirs::config_dir()`, a `home_dir()` or a read of `$HOME` anywhere
    /// else — test modules included, which the guard's panic does not cover —
    /// would be a route to the real files nobody sees.
    ///
    /// Allowed, each by its exact line: the resolver itself; the test pinning
    /// the paths existing installs use, which resolves and touches nothing;
    /// this test's own literals, one per line; and the SSH form expanding
    /// a `~/` the user typed in a key-file path, which resolves none of
    /// hubro's files.
    #[test]
    fn only_app_paths_asks_the_platform_for_a_directory() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        let mut allowed = vec![
            "Self::resolve(dirs::config_dir(), dirs::data_dir(), dirs::home_dir())",
            "let config = dirs::config_dir().expect(\"a config dir\").join(\"hubro\");",
            "let data = dirs::data_dir().expect(\"a data dir\").join(\"hubro\");",
            "let home = dirs::home_dir().expect(\"a home dir\");",
            "Some(rest) => match dirs::home_dir() {",
        ];
        // This test's own literals name the words too: the word list's
        // lines, and the allowed lines above as they are written here.
        let quoted: Vec<String> = PLATFORM_DIR_WORDS
            .iter()
            .chain(&allowed)
            .map(|text| format!("{text:?},"))
            .collect();
        allowed.extend(quoted.iter().map(String::as_str));
        let found = words_named_in(&root.join("src"), PLATFORM_DIR_WORDS, &allowed);
        assert!(
            found.is_empty(),
            "resolve hubro's files through AppPaths, not the platform's \
             directories directly:\n{}",
            found.join("\n")
        );
    }

    /// `tests/` links the library without `cfg(test)`, so the panic in
    /// [`AppPaths::platform`] does not reach it — and the library hands it
    /// every means to write a file: the savers (`save_theme`,
    /// `save_window_geometry`, `save_session`, the saved list's persist,
    /// `HistoryStore::open_at`) are public and take any path. So an
    /// integration test must not be able to *name* a real file: it may not
    /// compute a user's directory at all ([`PLATFORM_DIR_WORDS`]), nor name
    /// the paths, the state, or the `App` component, whose only constructor
    /// reachable from there resolves the real files. Every path it writes
    /// then comes from somewhere it made itself, such as a temp dir.
    #[test]
    fn no_integration_test_names_the_real_paths() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        let mut words = vec!["App", "AppState", "AppPaths", "from_platform_dirs"];
        words.extend(PLATFORM_DIR_WORDS);
        let found = words_named_in(&root.join("tests"), &words, &[]);
        assert!(
            found.is_empty(),
            "these can reach the user's real config and data; test through a \
             unit test's test_dom::provide_state, or write under a temp dir:\n{}",
            found.join("\n")
        );
    }
}
