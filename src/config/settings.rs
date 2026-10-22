//! User preferences: `$XDG_CONFIG_HOME/hubro/settings.toml`.
//!
//! Error policy: **lenient to read, careful to write**. These are
//! non-critical UI preferences, persisted separately from the connections
//! list so a corrupt settings file never blocks connecting to databases — a
//! missing *or* malformed file yields defaults instead of an error, and a
//! value this build cannot use costs only its own setting (see
//! [`interpret_settings`]). Those defaults are never written back over the
//! file, though: each `save_*` edits its one key in the user's document and
//! leaves the rest alone (see [`set_settings_key`]). What the load could not
//! use is reported as a [`SettingsProblem`], so the app can say why a
//! preference is not what the file says.

use std::path::{Path, PathBuf};

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use super::{write_text_atomic, ConfigError};

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
    /// tables roughly twenty to one. A file from before this flag loads it
    /// as off because [`interpret_settings`] defaults an absent key — the
    /// loader reads fields itself rather than through this struct's
    /// `Deserialize`, so the `serde(default)` here matters only to tests.
    /// `skip_serializing_if` is what keeps such a file free of the key.
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
/// never blocks the app — and one bad value yields the default for its own
/// setting only. [`load_settings_checked`] also says what was not used.
pub fn load_settings(path: &Path) -> Settings {
    load_settings_checked(path).0
}

/// Why the settings in effect differ from what settings.toml says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SettingsProblem {
    /// The file exists but could not be read as text (permissions, not
    /// UTF-8). Nothing in it applied, and no preference write will replace
    /// it.
    Unreadable(String),
    /// The file is not TOML. Nothing in it applied, and every preference
    /// write is refused until it parses again (see [`set_settings_key`]).
    /// `line` is 1-based, when the parser could place the error.
    NotToml {
        line: Option<usize>,
        message: String,
    },
    /// The file is TOML, but these keys (`window.width` for a field of a
    /// table) hold values this build cannot use. Each fell back to its
    /// default; every other setting applied, and writes work as usual.
    BadValues(Vec<String>),
}

impl SettingsProblem {
    /// Whether, with this problem, preference changes are not saved: the
    /// file is not one [`set_settings_key`] can edit. A bad value in a file
    /// that parses is not such a problem — changing the preference replaces
    /// it.
    pub fn saves_refused(&self) -> bool {
        match self {
            SettingsProblem::Unreadable(_) | SettingsProblem::NotToml { .. } => true,
            SettingsProblem::BadValues(_) => false,
        }
    }

    /// The one-line notice for the user, naming the file at `path`.
    pub fn notice(&self, path: &Path) -> String {
        let file = path.display();
        match self {
            SettingsProblem::Unreadable(err) => format!(
                "Preferences file {file} could not be read ({err}). Defaults are in use, \
                 and preference changes will not be saved until it can be read."
            ),
            SettingsProblem::NotToml { line, message } => {
                let at = line.map(|n| format!(" at line {n}")).unwrap_or_default();
                format!(
                    "Preferences file {file} is not valid TOML{at}: {message}. Defaults are \
                     in use, and preference changes will not be saved until it is fixed."
                )
            }
            SettingsProblem::BadValues(keys) => {
                let (subject, owner) = match keys.as_slice() {
                    [key] => (format!("the value of {key} is"), "its"),
                    keys => (format!("the values of {} are", keys.join(", ")), "their"),
                };
                format!(
                    "Preferences file {file}: {subject} not understood, so {owner} default \
                     is in use until the preference is changed here."
                )
            }
        }
    }
}

/// [`load_settings`], plus what it could not use. A missing file is not a
/// problem: it is how every install starts.
pub fn load_settings_checked(path: &Path) -> (Settings, Option<SettingsProblem>) {
    match std::fs::read_to_string(path) {
        Ok(text) => interpret_settings(&text),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => (Settings::default(), None),
        Err(err) => (
            Settings::default(),
            Some(SettingsProblem::Unreadable(err.to_string())),
        ),
    }
}

/// The settings `text` describes, field by field: a value this build cannot
/// use falls back to that field's default and is named in the returned
/// problem, and every other field keeps what the file says. Text that is not
/// TOML at all yields every default.
///
/// The struct is built as a literal with no `..Default::default()`, so a
/// field added to [`Settings`] does not compile until it is read here too —
/// otherwise it would silently always load as its default.
pub(crate) fn interpret_settings(text: &str) -> (Settings, Option<SettingsProblem>) {
    let table: toml::Table = match toml::from_str(text) {
        Ok(table) => table,
        Err(err) => {
            let line = err.span().map(|span| line_of(text, span.start));
            let problem = SettingsProblem::NotToml {
                line,
                message: one_line(err.message()),
            };
            return (Settings::default(), Some(problem));
        }
    };
    let mut bad = Vec::new();
    let settings = Settings {
        theme: field(&table, "", "theme", &mut bad).unwrap_or_default(),
        window: interpret_window(&table, &mut bad),
        show_internal_objects: field(&table, "", "show_internal_objects", &mut bad)
            .unwrap_or_default(),
    };
    let problem = (!bad.is_empty()).then_some(SettingsProblem::BadValues(bad));
    (settings, problem)
}

/// The `[window]` table, field by field like the top level: a bad `width`
/// costs the width, not the remembered position. A `window` that is not a
/// table at all is one bad value, and the window opens at the default size.
fn interpret_window(table: &toml::Table, bad: &mut Vec<String>) -> Option<WindowGeometry> {
    let window = match table.get("window")? {
        toml::Value::Table(window) => window,
        _ => {
            bad.push("window".to_string());
            return None;
        }
    };
    let bad = &mut *bad;
    Some(WindowGeometry {
        width: field(window, "window.", "width", bad).unwrap_or(DEFAULT_WINDOW_WIDTH),
        height: field(window, "window.", "height", bad).unwrap_or(DEFAULT_WINDOW_HEIGHT),
        x: field(window, "window.", "x", bad),
        y: field(window, "window.", "y", bad),
        maximized: field(window, "window.", "maximized", bad).unwrap_or_default(),
    })
}

/// `table[key]` as a `T`: `None` when the key is absent, and also when its
/// value is not a `T` — in which case `prefix` + `key` is added to `bad`.
fn field<T: DeserializeOwned>(
    table: &toml::Table,
    prefix: &str,
    key: &str,
    bad: &mut Vec<String>,
) -> Option<T> {
    let value = table.get(key)?.clone();
    match value.try_into() {
        Ok(value) => Some(value),
        Err(_) => {
            bad.push(format!("{prefix}{key}"));
            None
        }
    }
}

/// A parser's message, which can run over several lines (`invalid table
/// header` / ``expected `.`, `]` ``), as one line for a notice.
fn one_line(message: &str) -> String {
    message
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join("; ")
}

/// The 1-based line holding byte `offset` of `text`.
fn line_of(text: &str, offset: usize) -> usize {
    let offset = offset.min(text.len());
    text.as_bytes()[..offset]
        .iter()
        .filter(|&&b| b == b'\n')
        .count()
        + 1
}

/// Sets one top-level key of the settings file to `item`, or removes it
/// when `item` is `None`, leaving the rest of the document as it was.
///
/// For a key holding a table, `owned` names the fields this build writes
/// there, and only those are touched: each is set, or removed when `item`
/// leaves it out (a skipped `None`), while any other field the user's table
/// has — one a newer build added, say — stays. Replacing the table whole
/// would drop it, which is this function's whole reason to exist one level
/// down. Scalars pass an empty `owned`.
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
    owned: &[&str],
) -> Result<(), ConfigError> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(err) => return Err(ConfigError(format!("reading {}: {err}", path.display()))),
    };
    let mut document: toml_edit::Document = text.parse().map_err(|err: toml_edit::TomlError| {
        let at = err
            .span()
            .map(|span| format!(" at line {}", line_of(&text, span.start)))
            .unwrap_or_default();
        ConfigError(format!(
            "{} is not valid TOML{at} ({}), so it is left as it is",
            path.display(),
            one_line(err.message())
        ))
    })?;
    match item {
        Some(item) => put_item(document.as_table_mut(), key, item, owned),
        None => {
            document.remove(key);
        }
    }
    write_text_atomic(path, &document.to_string())
}

/// Puts `item` at `key` in `document`: merged field by field into a table
/// already there (whatever form it is written in — a `[key]` section, an
/// inline table, dotted keys) when `owned` names its fields, and otherwise
/// in place of what was there.
fn put_item(document: &mut toml_edit::Table, key: &str, mut item: toml_edit::Item, owned: &[&str]) {
    if !owned.is_empty() {
        if let (Some(new), Some(existing)) = (
            item.as_table_like(),
            document
                .get_mut(key)
                .and_then(toml_edit::Item::as_table_like_mut),
        ) {
            merge_fields(existing, new, owned);
            return;
        }
    }
    keep_layout(document.get(key), document.is_empty(), &mut item);
    document[key] = item;
}

/// Writes each `owned` field of `new` into `existing` — replacing a value in
/// its slot, so its key, place and trailing comment stay — and removes the
/// `owned` fields `new` leaves out. Fields outside `owned` are not touched.
fn merge_fields(
    existing: &mut dyn toml_edit::TableLike,
    new: &dyn toml_edit::TableLike,
    owned: &[&str],
) {
    for field in owned {
        let Some(value) = new.get(field) else {
            existing.remove(field);
            continue;
        };
        let mut value = value.clone();
        match existing.get_mut(field) {
            Some(slot) => {
                keep_layout(Some(slot), false, &mut value);
                *slot = value;
            }
            None => {
                existing.insert(field, value);
            }
        }
    }
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
    set_settings_key(path, "theme", Some(settings_item("theme", &theme)?), &[])
}

/// Persists just the internal-object visibility, leaving the rest of the
/// file as it is. Off removes the key rather than writing `false`, as the
/// field's `skip_serializing_if` does, so a file from before the flag gains
/// nothing until it is turned on.
pub fn save_show_internal_objects(path: &Path, show: bool) -> Result<(), ConfigError> {
    const KEY: &str = "show_internal_objects";
    let item = show.then(|| settings_item(KEY, &true)).transpose()?;
    set_settings_key(path, KEY, item, &[])
}

/// The fields of [`WindowGeometry`] as the file spells them: what a geometry
/// write owns inside `[window]`. Anything else in that table is left alone.
/// Pinned to the struct's serialization by
/// `window_fields_name_every_geometry_field`.
const WINDOW_FIELDS: [&str; 5] = ["width", "height", "x", "y", "maximized"];

/// Persists just the window geometry, leaving the rest of the file — the
/// rest of `[window]` included — as it is.
pub fn save_window_geometry(path: &Path, geometry: WindowGeometry) -> Result<(), ConfigError> {
    let item = settings_item("window", &geometry)?;
    set_settings_key(path, "window", Some(item), &WINDOW_FIELDS)
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
        // Every field non-default — a new one too — so a key the loader
        // misspells loads as its default and fails here
        // (`every_setting_reads_back_under_its_own_key` checks each alone).
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
    fn every_setting_reads_back_under_its_own_key() {
        let full = Settings {
            theme: Theme::Light,
            window: Some(WindowGeometry {
                width: 900.0,
                ..WindowGeometry::default()
            }),
            show_internal_objects: true,
        };
        // An exhaustive pattern, no `..`: a new field does not compile here
        // until it is named, given a non-default value above, and counted.
        let Settings {
            theme,
            window,
            show_internal_objects,
        } = &full;
        const FIELDS: usize = 3;
        assert_ne!(*theme, Theme::default());
        assert_ne!(*window, None);
        assert!(*show_internal_objects);

        // Each key as the struct serializes it — a `serde(rename)` included —
        // alone in a file, must be read by the loader: a key it spells
        // differently would load as the default.
        let toml::Value::Table(table) = toml::Value::try_from(&full).unwrap() else {
            panic!("settings serialize as a table");
        };
        assert_eq!(table.len(), FIELDS, "{table:?}");
        for (key, value) in table {
            let text = toml::to_string(&toml::Table::from_iter([(key.clone(), value)])).unwrap();
            let (loaded, problem) = interpret_settings(&text);
            assert_eq!(problem, None, "{key}");
            assert_ne!(
                loaded,
                Settings::default(),
                "the loader does not read {key}"
            );
        }
        assert_eq!(
            interpret_settings(&toml::to_string(&full).unwrap()),
            (full, None)
        );
    }

    #[test]
    fn only_a_file_that_cannot_be_edited_refuses_saves() {
        assert!(SettingsProblem::Unreadable("denied".into()).saves_refused());
        assert!(SettingsProblem::NotToml {
            line: Some(1),
            message: "bad".into()
        }
        .saves_refused());
        assert!(!SettingsProblem::BadValues(vec!["theme".into()]).saves_refused());
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

    /// A hand-edited file this build cannot wholly interpret: a comment, a key
    /// from some other build, and a typo [`load_settings`] cannot use —
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
        // The typo costs the theme, which is the lenient policy…
        assert_eq!(
            load_settings(&path),
            Settings {
                show_internal_objects: true,
                ..Settings::default()
            }
        );

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
    fn one_bad_value_costs_only_its_own_setting() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.toml");
        std::fs::write(&path, HAND_EDITED).unwrap();
        let (loaded, problem) = load_settings_checked(&path);
        // The good value beside the typo applies; the typo's own setting is
        // the default, and it is named — the unknown key is not a problem.
        assert_eq!(loaded.theme, Theme::System);
        assert!(loaded.show_internal_objects);
        assert_eq!(
            problem,
            Some(SettingsProblem::BadValues(vec!["theme".to_string()]))
        );
        assert_eq!(load_settings(&path), loaded);
    }

    #[test]
    fn a_bad_window_field_costs_only_that_field() {
        let (loaded, problem) = interpret_settings(
            "theme = \"dark\"\n\
             [window]\nwidth = \"wide\"\nheight = 600.0\nx = 10\nmaximized = true\nmonitor = 2\n",
        );
        assert_eq!(loaded.theme, Theme::Dark);
        assert_eq!(
            loaded.window,
            Some(WindowGeometry {
                width: DEFAULT_WINDOW_WIDTH,
                height: 600.0,
                x: Some(10.0),
                y: None,
                maximized: true,
            })
        );
        assert_eq!(
            problem,
            Some(SettingsProblem::BadValues(vec!["window.width".to_string()]))
        );

        // A `window` that is not a table at all opens at the default size.
        let (loaded, problem) =
            interpret_settings("window = 3\nshow_internal_objects = true\ntheme = 1\n");
        assert_eq!(loaded.window, None);
        assert!(loaded.show_internal_objects);
        assert_eq!(
            problem,
            Some(SettingsProblem::BadValues(vec![
                "theme".to_string(),
                "window".to_string()
            ]))
        );
    }

    #[test]
    fn a_bad_value_notice_names_the_file_and_every_key() {
        let path = Path::new("/cfg/settings.toml");
        let one = SettingsProblem::BadValues(vec!["theme".into()]).notice(path);
        assert!(one.contains("/cfg/settings.toml"), "{one}");
        assert!(one.contains("the value of theme is"), "{one}");
        let two =
            SettingsProblem::BadValues(vec!["theme".into(), "window.width".into()]).notice(path);
        assert!(two.contains("theme, window.width are"), "{two}");
    }

    #[test]
    fn a_file_this_build_fully_understands_is_no_problem() {
        // Unknown keys, inside `[window]` too, are a newer build's, not errors.
        let text = "theme = \"light\"\nshow_internal_objects = true\nlater = 1\n\
                    [window]\nwidth = 900.0\nheight = 600.0\nmonitor = 2\n";
        let (loaded, problem) = interpret_settings(text);
        assert_eq!(problem, None);
        assert_eq!(loaded.theme, Theme::Light);
        assert_eq!(interpret_settings(""), (Settings::default(), None));

        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("settings.toml");
        assert_eq!(load_settings_checked(&missing), (Settings::default(), None));
    }

    #[test]
    fn a_file_that_is_not_toml_is_reported_with_its_line() {
        let (loaded, problem) =
            interpret_settings("theme = \"dark\"\nshow_internal_objects = true\n[window\n");
        assert_eq!(loaded, Settings::default());
        let Some(SettingsProblem::NotToml { line, message }) = &problem else {
            panic!("not reported as unparseable: {problem:?}");
        };
        assert_eq!(*line, Some(3));
        assert!(!message.is_empty() && !message.contains('\n'), "{message}");

        let notice = problem.unwrap().notice(Path::new("/cfg/settings.toml"));
        assert!(notice.contains("/cfg/settings.toml"), "{notice}");
        assert!(notice.contains("line 3"), "{notice}");
        assert!(notice.contains("not be saved"), "{notice}");
    }

    #[test]
    fn a_file_that_cannot_be_read_is_reported() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.toml");
        std::fs::write(&path, [b't', b'h', 0xff, 0xfe, b'\n']).unwrap();
        let (loaded, problem) = load_settings_checked(&path);
        assert_eq!(loaded, Settings::default());
        assert!(
            matches!(problem, Some(SettingsProblem::Unreadable(_))),
            "{problem:?}"
        );
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
    fn window_fields_name_every_geometry_field() {
        // Every field set, so none is skipped from the serialization.
        let full = WindowGeometry {
            width: 1.0,
            height: 2.0,
            x: Some(3.0),
            y: Some(4.0),
            maximized: true,
        };
        let toml::Value::Table(table) = toml::Value::try_from(full).unwrap() else {
            panic!("geometry serializes as a table");
        };
        let mut serialized: Vec<&str> = table.keys().map(String::as_str).collect();
        let mut owned = WINDOW_FIELDS.to_vec();
        serialized.sort_unstable();
        owned.sort_unstable();
        assert_eq!(serialized, owned);
    }

    #[test]
    fn a_geometry_write_keeps_the_window_fields_it_does_not_own() {
        let geo = WindowGeometry {
            width: 900.0,
            height: 600.0,
            x: None,
            y: None,
            maximized: false,
        };
        // The same table in each of the forms TOML allows. A newer build's
        // field (`monitor`) must survive; a stale `x` the new geometry leaves
        // out must not.
        for (form, before, after) in [
            (
                "section",
                "[window]\nwidth = 1.0 # wide\nheight = 2.0\nx = 5.0\nmonitor = 2\n",
                "[window]\nwidth = 900.0 # wide\nheight = 600.0\nmonitor = 2\n",
            ),
            (
                "inline",
                "window = { width = 1.0, height = 2.0, x = 5.0, monitor = 2 }\n",
                "window = { width = 900.0, height = 600.0, monitor = 2 }\n",
            ),
            (
                "dotted",
                "window.width = 1.0\nwindow.height = 2.0\nwindow.x = 5.0\nwindow.monitor = 2\n",
                "window.width = 900.0\nwindow.height = 600.0\nwindow.monitor = 2\n",
            ),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("settings.toml");
            std::fs::write(&path, before).unwrap();
            save_window_geometry(&path, geo).unwrap();
            let written = std::fs::read_to_string(&path).unwrap();
            assert_eq!(written, after, "{form}");
            assert_eq!(load_settings(&path).window, Some(geo), "{form}");

            // A field the old table lacked is added in the same form, and
            // the result still reads back.
            let moved = WindowGeometry {
                x: Some(-40.0),
                maximized: true,
                ..geo
            };
            save_window_geometry(&path, moved).unwrap();
            let written = std::fs::read_to_string(&path).unwrap();
            assert!(written.contains("monitor = 2"), "{form}: {written}");
            assert_eq!(
                load_settings(&path).window,
                Some(moved),
                "{form}: {written}"
            );
        }
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
        // The refusal is one line naming the file and the error's line,
        // since it is what the user is shown.
        let ConfigError(refusal) = save_theme(&path, Theme::Dark).unwrap_err();
        assert!(!refusal.contains('\n'), "{refusal}");
        assert!(refusal.contains(&path.display().to_string()), "{refusal}");
        assert!(refusal.contains("line 3"), "{refusal}");

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
