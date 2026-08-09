//! User preferences: `$XDG_CONFIG_HOME/hubro/settings.toml`.
//!
//! Error policy: **lenient**. These are non-critical UI preferences,
//! persisted separately from the connections list so a corrupt settings file
//! never blocks connecting to databases — a missing *or* malformed file
//! yields defaults instead of an error.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::{write_toml_atomic, ConfigError};

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
    /// Whether the schema sidebar lists the objects a backend declared
    /// internal — extension schemas and tables, child partitions (FRE-88).
    /// Off by default: on a Timescale database they outnumber the user's
    /// tables roughly twenty to one. `default` + `skip_serializing_if` keep
    /// pre-FRE-88 settings files deserializing and unchanged on rewrite.
    #[serde(default, skip_serializing_if = "is_false")]
    pub show_internal_objects: bool,
}

/// `skip_serializing_if` predicate for `bool` fields defaulting to `false`.
fn is_false(value: &bool) -> bool {
    !*value
}

/// Default location: `$XDG_CONFIG_HOME/hubro/settings.toml`.
pub fn default_settings_path() -> Option<PathBuf> {
    Some(dirs::config_dir()?.join("hubro").join("settings.toml"))
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
    write_toml_atomic(path, settings)
}

/// Loads the current settings, applies `f`, and saves the result — the one
/// read-modify-write path behind every single-field `save_*`. Re-reading the
/// file first is what keeps a concurrent field from being lost: theme, window
/// geometry and sidebar visibility are written from different code paths.
pub fn update_settings(path: &Path, f: impl FnOnce(&mut Settings)) -> Result<(), ConfigError> {
    let mut settings = load_settings(path);
    f(&mut settings);
    save_settings(path, &settings)
}

/// Persists just the theme, preserving the rest of the settings file (see
/// [`update_settings`] for why the file is re-read first).
pub fn save_theme(path: &Path, theme: Theme) -> Result<(), ConfigError> {
    update_settings(path, |settings| settings.theme = theme)
}

/// Persists just the internal-object visibility (FRE-88), preserving the
/// rest (see [`update_settings`] for why the file is re-read first).
pub fn save_show_internal_objects(path: &Path, show: bool) -> Result<(), ConfigError> {
    update_settings(path, |settings| settings.show_internal_objects = show)
}

/// Persists just the window geometry, preserving the theme (see
/// [`update_settings`] for why the file is re-read first).
pub fn save_window_geometry(path: &Path, geometry: WindowGeometry) -> Result<(), ConfigError> {
    update_settings(path, |settings| settings.window = Some(geometry))
}

#[cfg(test)]
mod tests {
    use super::*;

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
            // Set, so the round trip pins a scalar declared after a table —
            // TOML would otherwise emit it inside `[window]`.
            show_internal_objects: true,
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
    fn system_schema_visibility_round_trips_and_stays_out_of_older_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.toml");

        // A pre-FRE-88 file loads with the flag off and, rewritten, gains no
        // key for it — an unaffected setting serializes unchanged.
        std::fs::write(&path, "theme = \"light\"\n").unwrap();
        assert!(!load_settings(&path).show_internal_objects);
        save_theme(&path, Theme::Dark).unwrap();
        assert!(!std::fs::read_to_string(&path)
            .unwrap()
            .contains("show_internal_objects"));

        save_show_internal_objects(&path, true).unwrap();
        let loaded = load_settings(&path);
        assert!(loaded.show_internal_objects);
        // Written alongside the theme rather than over it.
        assert_eq!(loaded.theme, Theme::Dark);
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
}
