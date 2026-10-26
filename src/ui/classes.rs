//! Class strings that more than one component uses, so a button, a field or
//! a status line looks the same everywhere it appears.
//!
//! The Tailwind scanner reads Rust source, doc comments included, so these
//! comments describe each control in words rather than naming utilities: a
//! utility named in prose would be emitted into the stylesheet whether or
//! not anything used it.
//!
//! A call site adds layout (margins, flex placement, a disabled look) next to
//! the constant, never a second size or colour: two utilities for the same
//! property resolve by stylesheet order, not by the order they are written
//! in, so an override would win or lose by accident.

/// A borderless text button in a toolbar: the grid's Copy, Export, Import,
/// Row detail and Refresh, and the theme toggle.
pub(super) const BTN_GHOST: &str = "rounded px-2 py-1 text-xs text-slate-500 dark:text-slate-400 hover:bg-slate-200 dark:hover:bg-slate-800 hover:text-slate-900 dark:hover:text-slate-100";

/// A square icon-only button on a connection row (protection, edit, sign
/// out, delete) and in the group headers.
pub(super) const BTN_ICON: &str = "cursor-pointer rounded p-1.5 text-slate-500 hover:bg-slate-300 dark:hover:bg-slate-700 hover:text-slate-900 dark:hover:text-slate-200";

/// A bordered, quiet button at toolbar or panel-header level: the editor's
/// Explain and panel toggles, "Save current query", "Clear history".
pub(super) const BTN_SECONDARY: &str = "rounded border border-slate-300 dark:border-slate-700 px-2 py-0.5 text-xs text-slate-500 dark:text-slate-400 hover:bg-slate-200 dark:hover:bg-slate-800 hover:text-slate-900 dark:hover:text-slate-100";

/// [`BTN_SECONDARY`] one step narrower, for an action on a single item: a
/// saved query's or history entry's Copy and Delete, a column's Rename, Show
/// DDL, Count exactly. Kept as its own size rather than folded into the
/// wider one, because the editor toolbar's toggles share the wider padding
/// with their pressed and disabled faces, and a toggle that changed width
/// when pressed would shift its neighbours.
pub(super) const BTN_SECONDARY_SM: &str = "rounded border border-slate-300 dark:border-slate-700 px-1.5 py-0.5 text-xs text-slate-500 dark:text-slate-400 hover:bg-slate-200 dark:hover:bg-slate-800 hover:text-slate-900 dark:hover:text-slate-100";

/// The shape of a filled, form-sized call to action, without its colour:
/// each caller adds a fill and its hover shade, because the colour carries
/// meaning (an engine's accent, a destructive confirm).
pub(super) const BTN_PRIMARY: &str = "rounded px-4 py-2 text-sm font-medium text-white";

/// The quiet Cancel / Close beside a [`BTN_PRIMARY`] in a form's footer.
pub(super) const BTN_CANCEL: &str = "rounded px-3 py-2 text-sm text-slate-500 dark:text-slate-400 hover:text-slate-900 dark:hover:text-slate-100";

/// A compact text field: panel forms, search boxes, the grid's filter value,
/// and the schema-editing dialog, its SQL box included.
pub(super) const INPUT: &str = "rounded border border-slate-300 dark:border-slate-700 bg-slate-100 dark:bg-slate-950 px-2 py-1 text-xs text-slate-900 dark:text-slate-200 placeholder:text-slate-400 dark:placeholder:text-slate-600";

/// A compact native select, matching [`INPUT`]: the grid's filter column and
/// operator, the import dialog's mapping.
pub(super) const SELECT: &str = "rounded border border-slate-300 dark:border-slate-700 bg-slate-100 dark:bg-slate-950 px-2 py-1 text-xs text-slate-900 dark:text-slate-200";

/// A connection form's text field: larger than [`INPUT`], and monospaced,
/// because what goes in it (hosts, ports, user names, paths) is read
/// character by character.
pub(super) const FORM_INPUT: &str = "w-full rounded border border-slate-300 dark:border-slate-700 bg-slate-100 dark:bg-slate-950 px-3 py-2 font-mono text-sm text-slate-900 dark:text-slate-200 placeholder:text-slate-400 dark:placeholder:text-slate-600";

/// A connection form's select, at [`FORM_INPUT`]'s height.
pub(super) const FORM_SELECT: &str = "rounded border border-slate-300 dark:border-slate-700 bg-slate-100 dark:bg-slate-950 px-2 py-2 text-sm text-slate-900 dark:text-slate-200";

/// How a one-line status reads: the export, import, saved-query, schema-edit
/// and copy outcomes. The text says what happened; the tone says whether it
/// needs looking at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    /// In progress, or neutral.
    Muted,
    /// Finished as asked.
    Ok,
    /// Finished, but not entirely as asked (rows skipped, defaults taken,
    /// cancelled).
    Warn,
    /// Failed.
    Error,
}

impl Tone {
    /// The text colour, for both themes.
    pub fn class(self) -> &'static str {
        match self {
            Tone::Muted => "text-slate-500 dark:text-slate-400",
            Tone::Ok => "text-emerald-700 dark:text-emerald-400",
            Tone::Warn => "text-amber-700 dark:text-amber-400",
            Tone::Error => "text-red-600 dark:text-red-400",
        }
    }
}
