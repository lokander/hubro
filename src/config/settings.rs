//! User preferences: `$XDG_CONFIG_HOME/hubro/settings.toml`.
//!
//! Error policy: **lenient to read, careful to write**. These are
//! non-critical UI preferences, persisted separately from the connections
//! list so a corrupt settings file never blocks connecting to databases — a
//! missing *or* malformed file yields defaults instead of an error. Those
//! defaults are never written back over the file, though: each `save_*`
//! edits its one key in the user's document and leaves the rest alone (see
//! [`set_settings_key`]).

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::{load_lenient, write_text_atomic, ConfigError};

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
    /// Last window size/position. `None` until the window is first
    /// resized/moved; on launch a missing value means "use the default size".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window: Option<WindowGeometry>,
    /// Whether the schema sidebar lists the objects a backend declared
    /// internal — extension schemas and tables, child partitions.
    /// Off by default: on a Timescale database they outnumber the user's
    /// tables roughly twenty to one. `default` + `skip_serializing_if` keep
    /// settings files from before this flag existed deserializing and
    /// unchanged on rewrite.
    #[serde(default, skip_serializing_if = "is_false")]
    pub show_internal_objects: bool,
}

/// `skip_serializing_if` predicate for `bool` fields defaulting to `false`.
fn is_false(value: &bool) -> bool {
    !*value
}

/// Default location: `$XDG_CONFIG_HOME/hubro/settings.toml`.
pub fn default_settings_path() -> Option<PathBuf> {
    super::app_config_file("settings.toml")
}

/// Loads settings. A missing *or* malformed file yields defaults — these are
/// non-critical UI preferences, so (unlike the connections list) a bad file
/// never surfaces an error or blocks the app; the user just gets defaults.
pub fn load_settings(path: &Path) -> Settings {
    load_lenient(path)
}

/// Sets one top-level key of the settings file to `value`, or removes it
/// when `value` is `None`, leaving the rest of the document as it was.
///
/// This is how every `save_*` below writes, and it is deliberately *not*
/// "load the struct, change a field, save the struct". [`load_settings`] is
/// lenient, so a file it could not interpret comes back as defaults, and
/// writing those back is how one hand-edited typo (`theme = "drak"`) used to
/// cost the user every other setting on the next window move. Editing the
/// document instead keeps whatever this build cannot interpret — a bad value,
/// a key from a newer build, a comment — exactly as the user left it.
///
/// A file that is not TOML at all cannot be edited, so it is not replaced
/// either: the write is refused and the file left untouched, the trade
/// `SavedList` makes for connections. The change still applies for the rest
/// of the session; it is just not persisted until the file parses again.
fn set_settings_key(
    path: &Path,
    key: &str,
    item: Option<toml_edit::Item>,
) -> Result<(), ConfigError> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(err) => return Err(ConfigError(format!("reading {}: {err}", path.display()))),
    };
    let mut document: toml_edit::Document = text.parse().map_err(|err| {
        ConfigError(format!(
            "{}: not rewriting a file that does not parse: {err}",
            path.display()
        ))
    })?;
    match item {
        Some(mut item) => {
            keep_layout(document.get(key), document.is_empty(), &mut item);
            document[key] = item;
        }
        None => {
            document.remove(key);
        }
    }
    write_text_atomic(path, &document.to_string())
}

/// Gives a replacement `item` the place and surroundings of the one it
/// replaces — a trailing comment on a value, the blank line above a
/// section, a section's position among the others — so an edit changes the
/// value and nothing else. A section added to a non-empty file gets a blank
/// line above it.
fn keep_layout(old: Option<&toml_edit::Item>, empty: bool, item: &mut toml_edit::Item) {
    use toml_edit::Item;
    match (old, item) {
        (Some(Item::Value(old)), Item::Value(new)) => *new.decor_mut() = old.decor().clone(),
        (Some(Item::Table(old)), Item::Table(new)) => {
            *new.decor_mut() = old.decor().clone();
            if let Some(position) = old.position() {
                new.set_position(position);
            }
        }
        (None, Item::Table(new)) if !empty => new.decor_mut().set_prefix("\n"),
        _ => {}
    }
}

/// `value` as the document item for `key`, rendered as a whole-file save
/// would render it: a table becomes a `[key]` section rather than an inline
/// table, with its fields in declaration order.
fn settings_item<T: Serialize>(key: &str, value: &T) -> Result<toml_edit::Item, ConfigError> {
    let single = std::collections::BTreeMap::from([(key, value)]);
    let text = toml::to_string_pretty(&single).map_err(|err| ConfigError(err.to_string()))?;
    let mut rendered: toml_edit::Document = text
        .parse()
        .map_err(|err: toml_edit::TomlError| ConfigError(err.to_string()))?;
    rendered
        .remove(key)
        .ok_or_else(|| ConfigError(format!("rendering settings key {key}")))
}

/// Persists just the theme, leaving the rest of the file as it is.
pub fn save_theme(path: &Path, theme: Theme) -> Result<(), ConfigError> {
    set_settings_key(path, "theme", Some(settings_item("theme", &theme)?))
}

/// Persists just the internal-object visibility, leaving the rest of the
/// file as it is. Off removes the key rather than writing `false`, as the
/// field's `skip_serializing_if` does, so a file from before the flag gains
/// nothing until it is turned on.
pub fn save_show_internal_objects(path: &Path, show: bool) -> Result<(), ConfigError> {
    const KEY: &str = "show_internal_objects";
    let item = show.then(|| settings_item(KEY, &true)).transpose()?;
    set_settings_key(path, KEY, item)
}

/// Persists just the window geometry, leaving the rest of the file as it is.
pub fn save_window_geometry(path: &Path, geometry: WindowGeometry) -> Result<(), ConfigError> {
    set_settings_key(path, "window", Some(settings_item("window", &geometry)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A whole-struct write, for round-trip tests. Not offered outside
    /// them: overwriting the file with a struct is the write
    /// [`set_settings_key`] exists to avoid.
    fn save_settings(path: &Path, settings: &Settings) -> Result<(), ConfigError> {
        super::super::write_toml_atomic(path, settings)
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
        // A theme-only file (as written before window state was persisted).
        std::fs::write(&path, "theme = \"light\"\n").unwrap();
        let loaded = load_settings(&path);
        assert_eq!(loaded.theme, Theme::Light);
        assert_eq!(loaded.window, None);
    }

    #[test]
    fn system_schema_visibility_round_trips_and_stays_out_of_older_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.toml");

        // A file from before the flag loads with it off and, rewritten, gains
        // no key for it — an unaffected setting serializes unchanged.
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

    /// A hand-edited file this build cannot interpret: a comment, a key from
    /// some other build, and a typo that sends [`load_settings`] to defaults —
    /// alongside a setting the user does want kept.
    const HAND_EDITED: &str = "# my settings\n\
                               theme = \"drak\"\n\
                               show_internal_objects = true\n\
                               from_a_newer_build = [1, 2]\n";

    #[test]
    fn a_setting_write_keeps_what_the_load_could_not_interpret() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.toml");
        std::fs::write(&path, HAND_EDITED).unwrap();
        // The typo costs the whole load, which is the lenient policy…
        assert_eq!(load_settings(&path), Settings::default());

        // …but not the file: a window move adds its section, set off by a
        // blank line, and changes nothing that was already there.
        let geo = WindowGeometry {
            width: 900.0,
            height: 600.0,
            x: None,
            y: None,
            maximized: false,
        };
        save_window_geometry(&path, geo).unwrap();
        let after_move = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            after_move,
            format!("{HAND_EDITED}\n[window]\nwidth = 900.0\nheight = 600.0\n")
        );

        // A theme write replaces the typo, and only the typo.
        save_theme(&path, Theme::Dark).unwrap();
        let after_theme = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            after_theme,
            after_move.replace("theme = \"drak\"", "theme = \"dark\""),
        );
        // With the typo gone, the setting the user kept is back in effect.
        let loaded = load_settings(&path);
        assert_eq!(loaded.theme, Theme::Dark);
        assert!(loaded.show_internal_objects);
        assert_eq!(loaded.window, Some(geo));
        assert!(after_theme.contains("from_a_newer_build = [1, 2]"));

        // Turning the flag off removes its key — the file's other lines stay.
        save_show_internal_objects(&path, false).unwrap();
        let after_off = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            after_off,
            after_theme.replace("show_internal_objects = true\n", "")
        );
        assert!(!load_settings(&path).show_internal_objects);
    }

    #[test]
    fn a_replaced_setting_keeps_its_comment_and_its_place() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.toml");
        std::fs::write(
            &path,
            "theme = \"light\" # for the office\n\n\
             [window]\nwidth = 1.0\nheight = 2.0\n\n\
             [later]\na = 1\n",
        )
        .unwrap();
        save_theme(&path, Theme::Dark).unwrap();
        save_window_geometry(&path, WindowGeometry::default()).unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "theme = \"dark\" # for the office\n\n\
             [window]\nwidth = 1200.0\nheight = 800.0\n\n\
             [later]\na = 1\n",
        );
    }

    #[test]
    fn a_settings_file_that_is_not_toml_is_never_overwritten() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.toml");
        let broken = "theme = \"dark\"\nshow_internal_objects = true\n[window\n";
        std::fs::write(&path, broken).unwrap();
        assert_eq!(load_settings(&path), Settings::default());

        assert!(save_window_geometry(&path, WindowGeometry::default()).is_err());
        assert!(save_theme(&path, Theme::Light).is_err());
        assert!(save_show_internal_objects(&path, false).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), broken.as_bytes());

        // Nor is one that cannot even be read as text: a read failure is not
        // a missing file, and must not be written over as if it were.
        let binary = [b't', b'h', 0xff, 0xfe, b'\n'];
        std::fs::write(&path, binary).unwrap();
        assert!(save_theme(&path, Theme::Light).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), binary);
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
