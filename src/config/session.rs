//! The last session: `$XDG_CONFIG_HOME/hubro/session.toml`.
//!
//! Error policy: **lenient**. Restore is best-effort and must never block or
//! crash startup — a missing *or* malformed file yields an empty session,
//! never an error. Kept separate from `settings.toml` because it is
//! transient, churns often, and is fine to lose — whereas a corrupt settings
//! file must not take user preferences with it.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::{write_toml_atomic, BackendKind, ConfigError};

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
///
/// Deliberately re-uses the derived impl rather than matching the variant
/// names by hand: a hand-written map would compile happily after a new
/// variant is added and silently read it back as the default, which is the
/// exact failure this helper exists to prevent.
fn pane_or_default<'de, D>(deserializer: D) -> Result<SessionPane, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de::IntoDeserializer;
    let raw = String::deserialize(deserializer)?;
    let by_name: serde::de::value::StringDeserializer<serde::de::value::Error> =
        raw.into_deserializer();
    Ok(SessionPane::deserialize(by_name).unwrap_or_default())
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
    /// Whether the row detail panel was docked open beside the grid
    /// (FRE-109). `default` + `skip_serializing_if` keep sessions written
    /// before it existed loading, and leave a tab that never opened it
    /// serializing exactly as before.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub row_detail: bool,
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

/// Default location: `$XDG_CONFIG_HOME/hubro/session.toml`. Kept separate
/// from `settings.toml` because it is transient, churns often, and is fine to
/// lose — whereas a corrupt settings file must not take user preferences with
/// it.
pub fn default_session_path() -> Option<PathBuf> {
    Some(dirs::config_dir()?.join("hubro").join("session.toml"))
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
    write_toml_atomic(path, session)
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

#[cfg(test)]
mod tests {
    use super::*;

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
                    row_detail: true,
                },
                SessionTab {
                    locator: "postgres://u@h:5432/app".into(),
                    selected_schema: Some("public".into()),
                    selected_table: Some("orders".into()),
                    pane: SessionPane::Sql,
                    row_detail: false,
                },
                SessionTab {
                    locator: "postgres://u@h:5432/other".into(),
                    selected_schema: None,
                    selected_table: Some("stock".into()),
                    pane: SessionPane::Schema,
                    row_detail: false,
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
    fn a_session_without_the_row_detail_key_loads_with_the_panel_closed() {
        // Files written before the row detail panel existed (FRE-109): the
        // tab must still load, with the panel simply closed.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.toml");
        std::fs::write(
            &path,
            "active = \"/data/music.db\"\n\n[[tabs]]\nlocator = \"/data/music.db\"\n\
             selected_table = \"artists\"\npane = \"browser\"\n",
        )
        .unwrap();
        let session = load_session(&path);
        assert_eq!(session.tabs.len(), 1);
        assert!(!session.tabs[0].row_detail);
        assert_eq!(session.tabs[0].selected_table.as_deref(), Some("artists"));
    }

    #[test]
    fn a_closed_row_detail_panel_writes_no_key_at_all() {
        // The unaffected-entries-serialize-unchanged half of the convention:
        // a tab that never opened the panel round-trips byte-identically to
        // what a pre-FRE-109 build wrote.
        let closed = Session {
            tabs: vec![SessionTab {
                locator: "/data/music.db".into(),
                selected_table: Some("artists".into()),
                ..Default::default()
            }],
            active: None,
        };
        let text = toml::to_string_pretty(&closed).unwrap();
        assert!(!text.contains("row_detail"), "{text}");
        // …and an open one is written, so it survives the next launch.
        let mut open = closed.clone();
        open.tabs[0].row_detail = true;
        let text = toml::to_string_pretty(&open).unwrap();
        assert!(text.contains("row_detail = true"), "{text}");
        assert_eq!(toml::from_str::<Session>(&text).unwrap(), open);
    }

    #[test]
    fn every_pane_variant_survives_the_tolerant_deserializer() {
        // Guards the reason `pane_or_default` delegates to the derive: each
        // variant must read back as itself, not quietly as the default.
        for pane in [SessionPane::Browser, SessionPane::Sql, SessionPane::Schema] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("session.toml");
            let session = Session {
                tabs: vec![SessionTab {
                    locator: "/db".into(),
                    pane,
                    ..Default::default()
                }],
                active: None,
            };
            save_session(&path, &session).unwrap();
            assert_eq!(load_session(&path).tabs[0].pane, pane, "{pane:?}");
        }
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
}
