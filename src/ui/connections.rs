//! The connections screen (FRE-67/FRE-75): the saved-connections list, the
//! add/edit connection forms behind their modal, and the cards that take over
//! when a connect parks on a prompt (password, SSH host key, Entra sign-in).
//!
//! Self-contained apart from [`AppState`]: nothing here is reachable from the
//! rest of the shell, which only renders [`ConnectionsScreen`] for
//! [`ActiveView::Connections`].

use std::path::PathBuf;

use dioxus::prelude::*;
use dioxus_icons::lucide::{
    ChevronDown, ChevronRight, ChevronUp, FolderPlus, Pencil, Plug, Search, ShieldAlert, Trash2, X,
};

use crate::config::{BackendKind, ConnectionColor, EditPrefill, SavedConnection, ServerAuth};
use crate::db::{Dialect, WriteProtection};

use super::icons::BackendIcon;
use super::js::focus_on_mount;
use super::notice::{Banner, BannerKind, EmptyState, Spinner};
use super::state::{ActiveView, AppState, ConnectStep, ServerBackend};

/// One saved connection's per-row settings, edited inline under its row in
/// the connections list: which group it is filed under (FRE-120), and its
/// write protection and accent colour (FRE-111).
///
/// Every change writes straight through to the saved list — there is no OK
/// button. These are small settings with an immediately visible effect (the
/// badge, the stripe and the row's section update in place), so a confirm
/// step would only add a way to lose the change.
///
/// Grouping shares this drawer rather than taking a per-row control of its
/// own: a second icon on every row costs more than the one line it saves,
/// and the two settings are asked the same way — pick one of a few values,
/// see it immediately.
#[component]
fn RowSettings(
    locator: String,
    protection: WriteProtection,
    color: Option<ConnectionColor>,
    /// The group this connection is in, if any.
    group: Option<String>,
) -> Element {
    let state = use_context::<AppState>();
    // Read here rather than passed down from the screen: at most one drawer
    // is open, so this is one clone of the group names per open drawer
    // instead of one per row per render.
    let groups: Vec<String> = state.saved.read().groups().to_vec();
    rsx! {
        div { class: "w-full border-t border-slate-200 dark:border-slate-800 bg-slate-100 dark:bg-slate-900/60 px-4 py-3",
            div { class: "mb-3 flex items-center gap-2",
                span { class: "text-xs font-medium text-slate-600 dark:text-slate-400", "Group" }
                if groups.is_empty() {
                    span { class: "text-xs text-slate-500",
                        "No groups yet — make one with “New group” above."
                    }
                } else {
                    select {
                        class: "rounded border border-slate-300 dark:border-slate-700 bg-slate-50 dark:bg-slate-950 px-2 py-1 text-xs text-slate-900 dark:text-slate-200",
                        aria_label: "Group",
                        onchange: {
                            let locator = locator.clone();
                            move |evt: FormEvent| {
                                // The empty value is "no group"; a group can
                                // never be named "" (create_group refuses it),
                                // so the two can't collide.
                                let picked = evt.value();
                                let picked = (!picked.is_empty()).then_some(picked);
                                state.assign_saved_group(&locator, picked.as_deref());
                            }
                        },
                        option { value: "", selected: group.is_none(), "No group" }
                        for name in groups.iter() {
                            option {
                                key: "{name}",
                                value: "{name}",
                                selected: group.as_deref() == Some(name.as_str()),
                                "{name}"
                            }
                        }
                    }
                }
            }
            div { class: "flex flex-wrap items-center gap-x-6 gap-y-3",
                div { class: "flex items-center gap-2",
                    span { class: "text-xs font-medium text-slate-600 dark:text-slate-400", "Writes" }
                    for option in [WriteProtection::Open, WriteProtection::Confirm, WriteProtection::ReadOnly] {
                        button {
                            key: "{option:?}",
                            class: if option == protection {
                                "cursor-pointer rounded bg-slate-700 dark:bg-slate-600 px-2 py-0.5 text-xs font-medium text-white"
                            } else {
                                "cursor-pointer rounded border border-slate-300 dark:border-slate-700 px-2 py-0.5 text-xs text-slate-600 dark:text-slate-400 hover:bg-slate-200 dark:hover:bg-slate-800"
                            },
                            aria_pressed: if option == protection { "true" } else { "false" },
                            onclick: {
                                let locator = locator.clone();
                                move |_| state.set_saved_marking(&locator, option, color)
                            },
                            "{protection_label(option)}"
                        }
                    }
                }
                div { class: "flex items-center gap-2",
                    span { class: "text-xs font-medium text-slate-600 dark:text-slate-400", "Colour" }
                    button {
                        class: if color.is_none() {
                            "cursor-pointer rounded border-2 border-slate-700 dark:border-slate-300 px-2 py-0.5 text-xs text-slate-700 dark:text-slate-300"
                        } else {
                            "cursor-pointer rounded border border-slate-300 dark:border-slate-700 px-2 py-0.5 text-xs text-slate-500 hover:bg-slate-200 dark:hover:bg-slate-800"
                        },
                        title: "No colour",
                        aria_label: "No colour",
                        aria_pressed: if color.is_none() { "true" } else { "false" },
                        onclick: {
                            let locator = locator.clone();
                            move |_| state.set_saved_marking(&locator, protection, None)
                        },
                        "None"
                    }
                    for swatch in ConnectionColor::ALL {
                        button {
                            key: "{swatch:?}",
                            // The selected swatch is ringed rather than
                            // ticked: a tick would have to be legible on six
                            // different backgrounds.
                            class: if color == Some(swatch) {
                                "size-5 cursor-pointer rounded-full ring-2 ring-slate-900 dark:ring-slate-100 ring-offset-2 ring-offset-slate-100 dark:ring-offset-slate-900"
                            } else {
                                "size-5 cursor-pointer rounded-full hover:ring-2 hover:ring-slate-400 hover:ring-offset-2 hover:ring-offset-slate-100 dark:hover:ring-offset-slate-900"
                            },
                            background_color: "{swatch.css()}",
                            title: "{swatch.label()}",
                            aria_label: "{swatch.label()}",
                            aria_pressed: if color == Some(swatch) { "true" } else { "false" },
                            onclick: {
                                let locator = locator.clone();
                                move |_| state.set_saved_marking(&locator, protection, Some(swatch))
                            },
                        }
                    }
                }
            }
            p { class: "mt-2 text-xs text-slate-500",
                "{protection_hint(protection)}"
            }
        }
    }
}

/// The button label for one protection state.
fn protection_label(protection: WriteProtection) -> &'static str {
    match protection {
        WriteProtection::Open => "Allow",
        WriteProtection::Confirm => "Confirm",
        WriteProtection::ReadOnly => "Refuse",
    }
}

/// One sentence saying what the chosen state actually does, so the three
/// buttons don't have to carry the whole explanation.
fn protection_hint(protection: WriteProtection) -> &'static str {
    match protection {
        WriteProtection::Open => "Writes run without extra confirmation.",
        WriteProtection::Confirm => {
            "Every write asks first, naming this connection. Colour only warns — it changes nothing."
        }
        // Deliberately not "outright": enforcement classifies statements, so
        // a write reached through a function call in a SELECT is not caught.
        // Promising more than that would be worse than promising less.
        WriteProtection::ReadOnly => {
            "Writes are refused, including from the SQL editor. \
             A write hidden inside a function call in a SELECT is not caught."
        }
    }
}

/// One row of the saved list, precomputed for rendering.
#[derive(Clone, PartialEq)]
struct SavedRow {
    name: String,
    locator: String,
    /// Canonical locator — the key open tabs and in-flight connects are
    /// tracked under. Differs from `locator` for a SQLite path that is not
    /// already canonical.
    key: String,
    backend: BackendKind,
    is_open: bool,
    /// The step of a connect in flight for this row, once it has run long
    /// enough to be worth showing. `None` means idle (or still too fast to
    /// report).
    connecting: Option<ConnectStep>,
    /// Whether that connect can be cancelled — true when it was started from
    /// this list rather than by submitting a form.
    cancellable: bool,
    /// The row's write protection and accent colour (FRE-111).
    protection: WriteProtection,
    color: Option<ConnectionColor>,
    /// The group this connection is filed under (FRE-120), if any.
    group: Option<String>,
    tunnel: Option<crate::tunnel::TunnelConfig>,
    auth: ServerAuth,
}

/// One section of the connections list as it renders (FRE-120): a group and
/// its rows, or — with `name` as `None` — the ungrouped ones.
///
/// `first`/`last` are about the group's place in the *configured* order
/// rather than in this (possibly searched) arrangement, because that is what
/// the reorder buttons act on.
#[derive(Clone, PartialEq)]
struct SectionView {
    name: Option<String>,
    rows: Vec<SavedRow>,
    collapsed: bool,
    first: bool,
    last: bool,
}

/// Whether a section renders folded (FRE-120).
///
/// **A search overrides the fold.** [`SavedList::arrange`] has already dropped
/// every section that matched nothing, so the ones left are hits; leaving one
/// folded would draw a header with a count and no rows — the same "headers
/// rather than hits" that `arrange` exists to prevent, and a dead end, since
/// the row the user searched for is the thing behind the fold.
///
/// The stored fold is only overridden, never cleared, so clearing the search
/// folds the group back up rather than costing the user the collapse they set.
/// The ungrouped section has no fold of its own — it is not a group.
fn section_collapsed(name: Option<&str>, collapsed: &[String], searching: bool) -> bool {
    match name {
        Some(name) if !searching => collapsed.iter().any(|folded| folded == name),
        _ => false,
    }
}

/// Creates the group named in the "New group" field, closing the field on
/// success and leaving it open (with the reason) on failure — a refused name
/// is still on screen to fix.
fn submit_new_group(
    state: AppState,
    name: Signal<String>,
    mut naming: Signal<bool>,
    mut error: Signal<Option<String>>,
) {
    match state.create_saved_group(&name.peek()) {
        Ok(_) => {
            error.set(None);
            naming.set(false);
        }
        Err(err) => error.set(Some(err.to_string())),
    }
}

/// One group's header: the fold control, its name and size, and the actions
/// that only make sense on a group — rename, reorder, delete (FRE-120).
///
/// Reordering is a pair of one-step buttons rather than drag-and-drop: a drag
/// implementation worth using is a dependency, and the thing being ordered is
/// a handful of names.
#[component]
fn GroupHeader(
    name: String,
    count: usize,
    collapsed: bool,
    /// Whether this group is already at the top / bottom of the configured
    /// order, which is what disables its Move button.
    first: bool,
    last: bool,
    renaming_group: Signal<Option<String>>,
    rename_draft: Signal<String>,
    confirm_delete_group: Signal<Option<String>>,
    group_error: Signal<Option<String>>,
) -> Element {
    let state = use_context::<AppState>();
    let mut renaming_group = renaming_group;
    let mut rename_draft = rename_draft;
    let mut confirm_delete_group = confirm_delete_group;
    let mut group_error = group_error;
    let renaming = renaming_group() == Some(name.clone());
    // Applies the rename, keeping the field open (with the reason) when the
    // name is refused.
    let commit_rename = {
        let name = name.clone();
        move || match state.rename_saved_group(&name, &rename_draft.peek()) {
            Ok(_) => {
                group_error.set(None);
                renaming_group.set(None);
            }
            Err(err) => group_error.set(Some(err.to_string())),
        }
    };
    rsx! {
        div { class: "flex items-center gap-1 border-b border-slate-200 dark:border-slate-800 bg-slate-100 dark:bg-slate-900/60 pr-2",
            if renaming {
                input {
                    class: "m-1.5 min-w-0 flex-1 rounded border border-slate-300 dark:border-slate-700 bg-slate-50 dark:bg-slate-950 px-2 py-1 text-sm text-slate-900 dark:text-slate-200",
                    value: "{rename_draft}",
                    onmounted: focus_on_mount,
                    oninput: move |evt| rename_draft.set(evt.value()),
                    onkeydown: {
                        let mut commit = commit_rename.clone();
                        move |evt: KeyboardEvent| match evt.key() {
                            Key::Enter => commit(),
                            Key::Escape => {
                                group_error.set(None);
                                renaming_group.set(None);
                            }
                            _ => {}
                        }
                    },
                }
                button {
                    class: "cursor-pointer rounded bg-sky-600 px-2 py-1 text-xs font-medium text-white hover:bg-sky-500",
                    onclick: {
                        let mut commit = commit_rename.clone();
                        move |_| commit()
                    },
                    "Rename"
                }
                button {
                    class: "cursor-pointer rounded px-2 py-1 text-xs text-slate-500 dark:text-slate-400 hover:text-slate-900 dark:hover:text-slate-100",
                    onclick: move |_| {
                        group_error.set(None);
                        renaming_group.set(None);
                    },
                    "Cancel"
                }
            } else if confirm_delete_group() == Some(name.clone()) {
                // Armed. Deleting a group is not deleting connections, and
                // the confirmation is where that gets said.
                div { class: "flex flex-1 flex-wrap items-center gap-2 px-3 py-1.5",
                    span { class: "text-xs text-amber-700 dark:text-amber-300",
                        "Delete “{name}”? Its connections stay, ungrouped."
                    }
                    button {
                        class: "cursor-pointer rounded bg-amber-600 px-2 py-0.5 text-xs font-semibold text-slate-950 hover:bg-amber-500",
                        onclick: {
                            let name = name.clone();
                            move |_| {
                                state.remove_saved_group(&name);
                                confirm_delete_group.set(None);
                            }
                        },
                        "Delete"
                    }
                    button {
                        class: "cursor-pointer rounded border border-slate-400 dark:border-slate-600 px-2 py-0.5 text-xs text-slate-900 dark:text-slate-300 hover:bg-slate-200 dark:hover:bg-slate-800",
                        onclick: move |_| confirm_delete_group.set(None),
                        "Keep"
                    }
                }
            } else {
                button {
                    class: "flex min-w-0 flex-1 cursor-pointer items-center gap-2 px-3 py-2 text-left hover:bg-slate-200 dark:hover:bg-slate-800/60",
                    title: if collapsed { "Expand this group" } else { "Collapse this group" },
                    aria_expanded: if collapsed { "false" } else { "true" },
                    onclick: {
                        let name = name.clone();
                        move |_| state.toggle_group_collapsed(&name)
                    },
                    span { class: "shrink-0 text-slate-500 dark:text-slate-400",
                        if collapsed {
                            ChevronRight { size: 14 }
                        } else {
                            ChevronDown { size: 14 }
                        }
                    }
                    span { class: "truncate text-sm font-semibold text-slate-900 dark:text-slate-200",
                        "{name}"
                    }
                    span { class: "shrink-0 text-xs text-slate-500 dark:text-slate-400", "{count}" }
                }
                button {
                    class: "cursor-pointer rounded p-1.5 text-slate-500 hover:bg-slate-300 dark:hover:bg-slate-700 hover:text-slate-900 dark:hover:text-slate-200",
                    title: "Rename group",
                    aria_label: "Rename group",
                    onclick: {
                        let name = name.clone();
                        move |_| {
                            group_error.set(None);
                            rename_draft.set(name.clone());
                            renaming_group.set(Some(name.clone()));
                        }
                    },
                    Pencil { size: 14 }
                }
                button {
                    class: if first {
                        "rounded p-1.5 text-slate-300 dark:text-slate-700"
                    } else {
                        "cursor-pointer rounded p-1.5 text-slate-500 hover:bg-slate-300 dark:hover:bg-slate-700 hover:text-slate-900 dark:hover:text-slate-200"
                    },
                    disabled: first,
                    title: "Move group up",
                    aria_label: "Move group up",
                    onclick: {
                        let name = name.clone();
                        move |_| state.move_saved_group(&name, true)
                    },
                    ChevronUp { size: 14 }
                }
                button {
                    class: if last {
                        "rounded p-1.5 text-slate-300 dark:text-slate-700"
                    } else {
                        "cursor-pointer rounded p-1.5 text-slate-500 hover:bg-slate-300 dark:hover:bg-slate-700 hover:text-slate-900 dark:hover:text-slate-200"
                    },
                    disabled: last,
                    title: "Move group down",
                    aria_label: "Move group down",
                    onclick: {
                        let name = name.clone();
                        move |_| state.move_saved_group(&name, false)
                    },
                    ChevronDown { size: 14 }
                }
                button {
                    class: "cursor-pointer rounded p-1.5 text-slate-500 hover:bg-slate-300 dark:hover:bg-slate-700 hover:text-slate-900 dark:hover:text-slate-200",
                    title: "Delete group",
                    aria_label: "Delete group",
                    onclick: {
                        let name = name.clone();
                        move |_| confirm_delete_group.set(Some(name.clone()))
                    },
                    Trash2 { size: 14 }
                }
            }
        }
    }
}

/// Which connection form the modal is showing (FRE-67). SQLite has no
/// form — it goes straight to the native file picker.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ConnectForm {
    Postgres,
    SqlServer,
    /// Editing an existing saved entry, prefilled from it (FRE-75). Carries
    /// the entry itself so the form knows which locator to replace — the
    /// edit may move it.
    ///
    /// Boxed because the entry dwarfs the other two variants, which carry
    /// nothing: unboxed, every `Option<ConnectForm>` in the screen would be
    /// the size of a whole saved connection.
    Edit(Box<SavedConnection>),
}

/// Modal shell for the add-connection forms (FRE-67). Follows the app's
/// overlay pattern (`fixed inset-0 z-50`, dimmed backdrop, centered panel)
/// with two deliberate differences from the stateless overlays (cheatsheet,
/// cell viewer):
///
/// - **A backdrop click does not dismiss.** These panels hold a half-typed
///   connection, so only Escape or an explicit Cancel/✕ closes them.
/// - **Escape is handled here, not by [`GLOBAL_KEYS_JS`].** That listener
///   ignores keys while a text field has focus, which is most of the time in
///   this form, so it would never fire for the modal. The keydown bubbles
///   from the fields to this container instead.
///
/// Note that the same Escape still reaches `GLOBAL_KEYS_JS`:
/// `dioxus-desktop` 0.7.9 serializes only `preventDefault` back to the
/// interpreter, so `stop_propagation` on a synthetic event does not stop the
/// real one. Harmless here — the only global Escape action is
/// `close_cheatsheet`, which no-ops unless the cheatsheet is open — and the
/// call below documents the intent for when that changes.
///
/// The overlay scrolls (`overflow-y-auto` with the panel's `my-auto` margins
/// collapsing on overflow) so the tall forms (auth plus SSH tunnel) stay
/// reachable in a small window without clipping at the top.
#[component]
fn ConnectFormModal(
    on_close: EventHandler<()>,
    /// A failed connect attempt for the form still on screen. Rendered here
    /// because the panel's own banner sits behind the backdrop, and the form
    /// deliberately stays open when a connect fails.
    error: Option<String>,
    on_dismiss_error: EventHandler<()>,
    children: Element,
) -> Element {
    rsx! {
        div {
            class: "fixed inset-0 z-50 flex items-start justify-center overflow-y-auto bg-black/40 p-4 outline-none",
            // Focused on mount so Escape works before the user clicks into a
            // field; keydowns from the fields bubble here either way.
            tabindex: "-1",
            onmounted: focus_on_mount,
            onkeydown: move |evt: KeyboardEvent| {
                if evt.code() == Code::Escape {
                    // Intent only — see the note above on why this does not
                    // actually stop the window listener today.
                    evt.stop_propagation();
                    on_close.call(());
                }
            },
            div { class: "my-auto w-full max-w-xl",
                div { class: "flex justify-end",
                    button {
                        class: "mb-1 rounded px-2 py-1 text-slate-300 hover:bg-white/10 hover:text-white",
                        aria_label: "Close",
                        onclick: move |_| on_close.call(()),
                        X { size: 16 }
                    }
                }
                if let Some(err) = error {
                    div { class: "mb-2 rounded bg-white dark:bg-slate-900",
                        Banner {
                            kind: BannerKind::Error,
                            message: err,
                            on_dismiss: move |_| on_dismiss_error.call(()),
                        }
                    }
                }
                {children}
            }
        }
    }
}

/// One saved connection's row. Lifted out of [`ConnectionsScreen`] when
/// grouping (FRE-120) put a second loop around it: the screen now walks
/// sections of rows, and 200 lines of row markup inside that walk would have
/// buried it.
///
/// The screen's signals are passed in rather than re-derived. A `Signal` is
/// `Copy`, so this costs nothing, and it keeps "one row armed, one editor
/// open" a property of the screen rather than something each row has to
/// agree about.
#[component]
fn SavedConnectionRow(
    row: SavedRow,
    confirm_remove: Signal<Option<String>>,
    settings_open: Signal<Option<String>>,
    open_form: Signal<Option<ConnectForm>>,
) -> Element {
    let state = use_context::<AppState>();
    let mut confirm_remove = confirm_remove;
    let mut settings_open = settings_open;
    let mut open_form = open_form;
    rsx! {
        // Same row-hover shade as the sidebar's table list. The
        // Edit/Remove buttons hover one step further so they
        // stay visible on top of it. Rounding the end rows
        // keeps the highlight inside the list's rounded border.
        //
        // The row's padding lives on the connect button, not on
        // the li: as li padding it was dead space that lit up on
        // hover but swallowed the click.
        li { class: "flex flex-wrap items-stretch first:rounded-t last:rounded-b hover:bg-slate-200 dark:hover:bg-slate-800/60",
            // Accent stripe (FRE-111): the colour warns, and
            // it reads before any text does.
            if let Some(color) = row.color {
                div {
                    class: "w-1 shrink-0 first:rounded-tl last:rounded-bl",
                    background_color: "{color.css()}",
                    aria_hidden: "true",
                }
            }
            button {
                // Dimmed and inert while its connect runs, so
                // the row reads as busy rather than ignored.
                class: if row.connecting.is_some() {
                    "min-w-0 flex-1 cursor-default px-4 py-3 text-left opacity-60"
                } else {
                    "min-w-0 flex-1 cursor-pointer px-4 py-3 text-left"
                },
                disabled: row.connecting.is_some(),
                title: "Shift-click to open in the background",
                onclick: {
                    let row = row.clone();
                    move |evt: MouseEvent| {
                        // Shift-click opens the tab without
                        // switching to it, for queueing up
                        // several connections at once.
                        state
                            .start_connect(
                                row.locator.clone(),
                                row.name.clone(),
                                row.backend,
                                row.tunnel.clone(),
                                row.auth.clone(),
                                !evt.modifiers().shift(),
                            );
                    }
                },
                div { class: "flex items-center gap-2",
                    span { class: "shrink-0 text-slate-500 dark:text-slate-400",
                        if row.connecting.is_some() {
                            Spinner {}
                        } else {
                            BackendIcon { dialect: Dialect::from(row.backend), size: 16 }
                        }
                    }
                    span { class: "truncate text-sm font-medium text-slate-900 dark:text-slate-200",
                        "{row.name}"
                    }
                    span {
                        class: match row.backend {
                            BackendKind::Postgres => "rounded bg-cyan-100 dark:bg-cyan-900/50 px-1.5 py-0.5 text-xs text-cyan-700 dark:text-cyan-300",
                            BackendKind::SqlServer => "rounded bg-red-100 dark:bg-red-900/50 px-1.5 py-0.5 text-xs text-red-700 dark:text-red-300",
                            // A step darker than the row's hover shade; at
                            // bg-slate-200 the badge disappeared into it.
                            BackendKind::Sqlite => "rounded bg-slate-300 dark:bg-slate-700 px-1.5 py-0.5 text-xs text-slate-600 dark:text-slate-300",
                        },
                        match row.backend {
                            BackendKind::Postgres => "postgres",
                            BackendKind::SqlServer => "sql server",
                            BackendKind::Sqlite => "sqlite",
                        }
                    }
                    if row.tunnel.is_some() {
                        span { class: "rounded bg-teal-100 dark:bg-teal-900/50 px-1.5 py-0.5 text-xs text-teal-700 dark:text-teal-300",
                            "ssh"
                        }
                    }
                    if row.is_open {
                        span { class: "rounded bg-sky-100 dark:bg-sky-900/60 px-1.5 py-0.5 text-xs text-sky-700 dark:text-sky-300",
                            "open"
                        }
                    }
                    // Protection is never silent (FRE-111):
                    // the user should never wonder why a save
                    // button is disabled or a prompt appeared.
                    if let Some(badge) = row.protection.badge() {
                        span { class: "rounded bg-amber-100 dark:bg-amber-900/50 px-1.5 py-0.5 text-xs text-amber-700 dark:text-amber-300",
                            "{badge}"
                        }
                    }
                }
                // The step replaces the locator while connecting:
                // the locator is already on screen in the name,
                // and the phase is what the user needs.
                //
                // One element for both, never swapped out: a
                // live region only announces changes made after
                // it exists, so a region created together with
                // the first step would stay silent for it.
                div {
                    class: if row.connecting.is_some() {
                        "truncate text-xs text-slate-500 dark:text-slate-400"
                    } else {
                        "truncate font-mono text-xs text-slate-500"
                    },
                    aria_live: "polite",
                    if let Some(step) = row.connecting {
                        "{step.label()}"
                    } else {
                        "{row.locator}"
                    }
                }
            }
            // Only the row actions sit outside the connect
            // button; everything left of them is one click
            // target spanning the full row height.
            // Icon-only to keep the row narrow; `title` carries
            // the label the text used to, and `aria_label`
            // keeps it named for screen readers.
            div { class: "flex shrink-0 items-center gap-1 pr-2",
                if row.connecting.is_some() {
                    // Editing or removing a connection mid-connect
                    // would fight the attempt in flight, so the
                    // only action offered is calling it off.
                    if row.cancellable {
                        button {
                            class: "cursor-pointer rounded p-1.5 text-slate-500 hover:bg-slate-300 dark:hover:bg-slate-700 hover:text-slate-900 dark:hover:text-slate-200",
                            title: "Cancel",
                            aria_label: "Cancel connecting",
                            onclick: {
                                let key = row.key.clone();
                                move |_| state.cancel_connect(&key)
                            },
                            X { size: 14 }
                        }
                    }
                } else if confirm_remove().as_deref() == Some(row.locator.as_str()) {
                    // Armed: the icons step aside for the
                    // confirmation, same shape as the editor's
                    // "Clear this connection's history?".
                    div { class: "flex items-center gap-2",
                        span { class: "text-xs text-amber-700 dark:text-amber-300", "Remove?" }
                        button {
                            class: "cursor-pointer rounded bg-amber-600 px-2 py-0.5 text-xs font-semibold text-slate-950 hover:bg-amber-500",
                            onclick: {
                                let locator = row.locator.clone();
                                move |_| {
                                    state.remove_saved(&locator);
                                    confirm_remove.set(None);
                                }
                            },
                            "Remove"
                        }
                        button {
                            class: "cursor-pointer rounded border border-slate-400 dark:border-slate-600 px-2 py-0.5 text-xs text-slate-900 dark:text-slate-300 hover:bg-slate-200 dark:hover:bg-slate-800",
                            onclick: move |_| confirm_remove.set(None),
                            "Keep"
                        }
                    }
                } else {
                    // Marking is offered for every backend,
                    // including SQLite — which has no edit
                    // form, so this is the only place it can
                    // be protected from.
                    button {
                        class: if settings_open() == Some(row.locator.clone()) {
                            "cursor-pointer rounded bg-slate-300 dark:bg-slate-700 p-1.5 text-slate-900 dark:text-slate-200"
                        } else {
                            "cursor-pointer rounded p-1.5 text-slate-500 hover:bg-slate-300 dark:hover:bg-slate-700 hover:text-slate-900 dark:hover:text-slate-200"
                        },
                        title: "Group, write protection and colour",
                        aria_label: "Group, write protection and colour",
                        aria_expanded: if settings_open() == Some(row.locator.clone()) { "true" } else { "false" },
                        onclick: {
                            let locator = row.locator.clone();
                            move |_| {
                                let open = settings_open() == Some(locator.clone());
                                settings_open.set((!open).then(|| locator.clone()));
                            }
                        },
                        ShieldAlert { size: 14 }
                    }
                    if row.backend != BackendKind::Sqlite {
                        button {
                            class: "cursor-pointer rounded p-1.5 text-slate-500 hover:bg-slate-300 dark:hover:bg-slate-700 hover:text-slate-900 dark:hover:text-slate-200",
                            title: "Edit",
                            aria_label: "Edit saved connection",
                            onclick: {
                                let locator = row.locator.clone();
                                move |_| {
                                    let entry = state
                                        .saved
                                        .read()
                                        .entries()
                                        .iter()
                                        .find(|s| s.locator() == locator)
                                        .cloned();
                                    if let Some(entry) = entry {
                                        open_form.set(Some(ConnectForm::Edit(Box::new(entry))));
                                    }
                                }
                            },
                            Pencil { size: 14 }
                        }
                    }
                    button {
                        class: "cursor-pointer rounded p-1.5 text-slate-500 hover:bg-slate-300 dark:hover:bg-slate-700 hover:text-slate-900 dark:hover:text-slate-200",
                        title: "Remove",
                        aria_label: "Remove saved connection",
                        onclick: {
                            let locator = row.locator.clone();
                            move |_| confirm_remove.set(Some(locator.clone()))
                        },
                        Trash2 { size: 14 }
                    }
                }
            }
            // The settings editor (FRE-111, FRE-120), expanded
            // inline below its row — the row is already a
            // full-width click target, so a floating popover
            // would have to be dismissed before the row could
            // be used.
            if settings_open() == Some(row.locator.clone()) {
                RowSettings {
                    locator: row.locator.clone(),
                    protection: row.protection,
                    color: row.color,
                    group: row.group.clone(),
                }
            }
        }
    }
}

/// Launch screen: the persisted saved-connections list plus add flows for
/// SQLite (native file picker) and Postgres (form or URL).
#[component]
pub(super) fn ConnectionsScreen() -> Element {
    let state = use_context::<AppState>();
    // One open form at a time (they were a mutually-exclusive bool pair
    // before FRE-67); `None` means no modal is up.
    let mut open_form = use_signal(|| Option::<ConnectForm>::None);
    // Locator of the row whose Remove is armed, if any. Removing is instant
    // and unrecoverable (a server entry takes its keyring password with it),
    // so the trash icon only arms the confirmation — one row at a time, since
    // arming another replaces this.
    let confirm_remove = use_signal(|| Option::<String>::None);
    // Which row's settings editor is expanded: group (FRE-120), write
    // protection and colour (FRE-111).
    let settings_open = use_signal(|| Option::<String>::None);
    // The connections search (FRE-120). Local to this screen, like the
    // sidebar's filter: it narrows a view rather than changing anything, so
    // it is not persisted and the screen opens unfiltered.
    let mut search = use_signal(String::new);
    // The "New group" field, shown only while it is being typed into.
    let mut naming_group = use_signal(|| false);
    let mut new_group = use_signal(String::new);
    // Which group's name is being edited, and the draft (FRE-120).
    let renaming_group = use_signal(|| Option::<String>::None);
    let rename_draft = use_signal(String::new);
    // Which group's Delete is armed — same one-at-a-time arming as a row's.
    let confirm_delete_group = use_signal(|| Option::<String>::None);
    // Why the last group name was refused, shown under the field it was
    // typed into.
    let mut group_error = use_signal(|| Option::<String>::None);
    let error = state.connect_error.read().clone();
    let prompt = state.password_prompt.read().clone();
    let host_key_prompt = state.host_key_prompt.read().clone();
    let entra_prompt = state.entra_prompt.read().clone();
    // A connect that parks on a prompt — password, SSH passphrase, host-key
    // trust, Entra sign-in — hands the flow to that card, which renders in
    // the panel. The form modal has to step aside for it: left up, the card
    // would sit invisible behind the backdrop, and its autofocus would pull
    // focus out of the modal (taking Escape with it). The Entra branch of
    // the submit path already closes the form for exactly this reason.
    use_effect(move || {
        let pending = state.password_prompt.read().is_some()
            || state.host_key_prompt.read().is_some()
            || state.entra_prompt.read().is_some();
        if pending && open_form.peek().is_some() {
            open_form.set(None);
        }
    });
    let query = search();
    // The same test `arrange` applies, so the fold override below and the
    // section filtering can't disagree about what "searching" means.
    let searching = !query.trim().is_empty();
    let groups: Vec<String> = state.saved.read().groups().to_vec();
    let collapsed: Vec<String> = state.collapsed_groups.read().clone();
    // Whether anything is saved at all — the empty state's condition, which
    // a search that matches nothing must not trigger.
    let has_saved = !state.saved.read().entries().is_empty();
    // The list as it renders (FRE-120): each group in its configured order
    // with the connections filed under it, then the ungrouped ones, narrowed
    // by the search box. `arrange` owns those two rules so the render body
    // stays a straight walk over what it returned.
    let sections: Vec<SectionView> = {
        let open = state.open_locators.read();
        let connecting = state.connecting.read();
        let requests = state.connect_requests.read();
        let row_for = |s: &SavedConnection| {
            let canonical_locator = super::state::saved_open_locator(s);
            let (tunnel, auth) = match s {
                SavedConnection::Postgres { tunnel, auth, .. }
                | SavedConnection::SqlServer { tunnel, auth, .. } => (tunnel.clone(), auth.clone()),
                SavedConnection::Sqlite { .. } => (None, ServerAuth::Password),
            };
            SavedRow {
                name: s.name().to_string(),
                locator: s.locator(),
                key: canonical_locator.clone(),
                backend: s.backend(),
                is_open: open.iter().any(|(_, l)| *l == canonical_locator),
                connecting: connecting
                    .iter()
                    .find(|c| c.visible && c.locator == canonical_locator)
                    .map(|c| c.step),
                cancellable: requests.contains_key(&canonical_locator),
                protection: s.protection(),
                color: s.color(),
                group: s.group().map(str::to_string),
                tunnel,
                auth,
            }
        };
        state
            .saved
            .read()
            .arrange(&query)
            .into_iter()
            .map(|section| SectionView {
                collapsed: section_collapsed(section.name.as_deref(), &collapsed, searching),
                // Whether the up/down buttons have anywhere to go is asked of
                // the *configured* order, not of this arrangement: a search
                // hides sections, and a Move Up that greyed out because the
                // group above it was filtered away would move the group
                // somewhere the user can't see.
                first: groups.first() == section.name.as_ref(),
                last: groups.last() == section.name.as_ref(),
                rows: section.entries.iter().map(&row_for).collect(),
                name: section.name,
            })
            .collect()
    };

    let pick_file = move |_| {
        spawn(async move {
            let picked = rfd::AsyncFileDialog::new()
                .set_title("Add a SQLite database")
                .add_filter("SQLite databases", &["db", "sqlite", "sqlite3"])
                .add_filter("All files", &["*"])
                .pick_file()
                .await;
            if let Some(file) = picked {
                state.add_saved(file.path().to_path_buf());
            }
        });
    };

    rsx! {
        div { class: "flex h-full flex-col items-center justify-center gap-6 overflow-y-auto py-8",
            div { class: "text-center",
                h1 { class: "text-2xl font-semibold text-slate-900 dark:text-slate-200", "Hubro" }
                if has_saved {
                    p { class: "mt-1 text-sm text-slate-500 dark:text-slate-400",
                        "Pick a saved connection, or add another database."
                    }
                }
            }
            if !has_saved {
                // Designed empty state; the Add buttons below are its action.
                EmptyState {
                    icon: rsx! { Plug { size: 40 } },
                    title: "No connections yet",
                    hint: "Add a SQLite file, Postgres server, or SQL Server to get started.",
                }
            }
            // Search and group creation (FRE-120). Both only appear once
            // there is a list to organise — on an empty screen they would be
            // two controls with nothing to act on.
            if has_saved {
                div { class: "flex w-full max-w-xl flex-col gap-2",
                    div { class: "flex items-center gap-2",
                        div { class: "flex min-w-0 flex-1 items-center gap-1 rounded border border-slate-300 dark:border-slate-700 bg-slate-100 dark:bg-slate-950 px-2 py-1 focus-within:border-sky-500 dark:focus-within:border-sky-600",
                            span { class: "shrink-0 text-slate-400 dark:text-slate-600",
                                Search { size: 14 }
                            }
                            input {
                                id: "dv-connection-search",
                                class: "min-w-0 flex-1 bg-transparent text-sm text-slate-900 dark:text-slate-200 placeholder:text-slate-400 dark:placeholder:text-slate-600 focus:outline-none",
                                placeholder: "Search connections by name…",
                                value: "{query}",
                                oninput: move |evt| search.set(evt.value()),
                                // Handled here, not in the window listener,
                                // which ignores keys typed into an input.
                                onkeydown: move |evt: KeyboardEvent| {
                                    if evt.key() == Key::Escape {
                                        search.set(String::new());
                                    }
                                },
                            }
                            if !query.is_empty() {
                                button {
                                    class: "shrink-0 rounded text-slate-400 dark:text-slate-600 hover:text-slate-900 dark:hover:text-slate-200",
                                    title: "Clear the search (Esc)",
                                    aria_label: "Clear the search",
                                    onclick: move |_| search.set(String::new()),
                                    X { size: 14 }
                                }
                            }
                        }
                        button {
                            class: "flex shrink-0 cursor-pointer items-center gap-1 rounded border border-slate-300 dark:border-slate-700 px-2 py-1.5 text-xs text-slate-600 dark:text-slate-400 hover:bg-slate-200 dark:hover:bg-slate-800 hover:text-slate-900 dark:hover:text-slate-100",
                            title: "Create a group to file connections under",
                            aria_expanded: if naming_group() { "true" } else { "false" },
                            onclick: move |_| {
                                let open = naming_group();
                                group_error.set(None);
                                new_group.set(String::new());
                                naming_group.set(!open);
                            },
                            FolderPlus { size: 14 }
                            "New group"
                        }
                    }
                    if naming_group() {
                        div { class: "flex items-center gap-2",
                            input {
                                class: "min-w-0 flex-1 rounded border border-slate-300 dark:border-slate-700 bg-slate-100 dark:bg-slate-950 px-2 py-1 text-sm text-slate-900 dark:text-slate-200",
                                placeholder: "Group name, e.g. Production",
                                value: "{new_group}",
                                onmounted: focus_on_mount,
                                oninput: move |evt| new_group.set(evt.value()),
                                onkeydown: move |evt: KeyboardEvent| {
                                    match evt.key() {
                                        Key::Enter => {
                                            submit_new_group(state, new_group, naming_group, group_error)
                                        }
                                        Key::Escape => {
                                            group_error.set(None);
                                            naming_group.set(false);
                                        }
                                        _ => {}
                                    }
                                },
                            }
                            button {
                                class: "cursor-pointer rounded bg-sky-600 px-3 py-1 text-xs font-medium text-white hover:bg-sky-500",
                                onclick: move |_| {
                                    submit_new_group(state, new_group, naming_group, group_error)
                                },
                                "Create"
                            }
                            button {
                                class: "cursor-pointer rounded px-2 py-1 text-xs text-slate-500 dark:text-slate-400 hover:text-slate-900 dark:hover:text-slate-100",
                                onclick: move |_| {
                                    group_error.set(None);
                                    naming_group.set(false);
                                },
                                "Cancel"
                            }
                        }
                    }
                    if let Some(err) = group_error() {
                        p { class: "text-xs text-red-600 dark:text-red-400", "{err}" }
                    }
                }
            }
            // A search that matches nothing says so, rather than looking like
            // an empty connections list.
            if has_saved && sections.is_empty() {
                p { class: "text-sm text-slate-500 dark:text-slate-400",
                    "No connection matches “{query}”."
                }
            }
            for section in sections {
                // One bordered block per section, so a group reads as
                // containing its rows rather than sitting above them.
                div {
                    key: "{section.name:?}",
                    class: "w-full max-w-xl overflow-hidden rounded border border-slate-300 dark:border-slate-700 bg-slate-50 dark:bg-slate-950/60",
                    if let Some(name) = section.name.clone() {
                        GroupHeader {
                            name,
                            count: section.rows.len(),
                            collapsed: section.collapsed,
                            first: section.first,
                            last: section.last,
                            renaming_group,
                            rename_draft,
                            confirm_delete_group,
                            group_error,
                        }
                    }
                    // The ungrouped rows are labelled only once a group
                    // exists: with no groups at all the list is exactly the
                    // flat one it has always been, header and all.
                    if section.name.is_none() && !groups.is_empty() {
                        div { class: "border-b border-slate-200 dark:border-slate-800 px-3 py-1.5 text-xs font-semibold uppercase tracking-wide text-slate-500",
                            "Ungrouped"
                        }
                    }
                    if !section.collapsed {
                        // The emptiness check comes first so the loop below
                        // can *move* the rows instead of cloning every row of
                        // every section on every render.
                        if section.rows.is_empty() {
                            p { class: "px-4 py-3 text-xs text-slate-500 dark:text-slate-400",
                                "No connections here yet — open a connection's shield button and pick this group."
                            }
                        }
                        ul { class: "divide-y divide-slate-200 dark:divide-slate-800",
                            for row in section.rows {
                                SavedConnectionRow {
                                    key: "{row.locator}",
                                    row,
                                    confirm_remove,
                                    settings_open,
                                    open_form,
                                }
                            }
                        }
                    }
                }
            }
            if let Some(prompt) = prompt {
                // Keyed on kind too: moving from the SSH-passphrase prompt to
                // the db-password prompt for the same URL resets the input.
                PasswordPromptCard { key: "{prompt.url}:{prompt.kind:?}", prompt }
            }
            if let Some(host_key_prompt) = host_key_prompt {
                HostKeyPromptCard {
                    key: "{host_key_prompt.url}:{host_key_prompt.info.fingerprint}",
                    prompt: host_key_prompt,
                }
            }
            if let Some(entra_prompt) = entra_prompt {
                EntraSignInCard { key: "{entra_prompt.url}", prompt: entra_prompt }
            }
            div { class: "flex gap-3",
                button {
                    class: "flex items-center gap-2 rounded bg-sky-600 px-4 py-2 text-sm font-medium text-white hover:bg-sky-500",
                    onclick: pick_file,
                    BackendIcon { dialect: Dialect::Sqlite, size: 16 }
                    "Add SQLite file…"
                }
                button {
                    class: "flex items-center gap-2 rounded bg-cyan-700 px-4 py-2 text-sm font-medium text-white hover:bg-cyan-600",
                    onclick: move |_| open_form.set(Some(ConnectForm::Postgres)),
                    BackendIcon { dialect: Dialect::Postgres, size: 16 }
                    "Add Postgres…"
                }
                button {
                    class: "flex items-center gap-2 rounded bg-red-800 px-4 py-2 text-sm font-medium text-white hover:bg-red-700",
                    onclick: move |_| open_form.set(Some(ConnectForm::SqlServer)),
                    BackendIcon { dialect: Dialect::SqlServer, size: 16 }
                    "Add SQL Server…"
                }
            }
            if let Some(kind) = open_form() {
                ConnectFormModal {
                    on_close: move |_| {
                        state.clear_pending_edit();
                        open_form.set(None);
                    },
                    error: error.clone(),
                    on_dismiss_error: move |_| state.connect_error.clone().set(None),
                    match kind {
                        ConnectForm::Postgres => rsx! {
                            PostgresForm { on_done: move |_| open_form.set(None) }
                        },
                        ConnectForm::SqlServer => rsx! {
                            SqlServerForm { on_done: move |_| open_form.set(None) }
                        },
                        // Only server connections reach here; the Edit action
                        // isn't offered on SQLite entries.
                        ConnectForm::Edit(saved) if matches!(*saved, SavedConnection::SqlServer { .. }) => rsx! {
                            SqlServerForm {
                                edit: (*saved).clone(),
                                on_done: move |_| open_form.set(None),
                            }
                        },
                        ConnectForm::Edit(saved) => rsx! {
                            PostgresForm {
                                edit: (*saved).clone(),
                                on_done: move |_| open_form.set(None),
                            }
                        },
                    }
                }
            }
            // Only when no modal is up — the modal renders it itself, so it
            // isn't hidden behind the backdrop.
            if let (Some(err), None) = (error, open_form()) {
                div { class: "w-full max-w-xl px-8",
                    Banner {
                        kind: BannerKind::Error,
                        message: err,
                        on_dismiss: move |_| state.connect_error.clone().set(None),
                    }
                }
            }
        }
    }
}

/// Cancel for the three connect prompt cards (password/passphrase, host-key
/// trust, Entra sign-in): clears the card, and with it any saved-connection
/// edit still waiting for the parked connect to confirm it (FRE-75) — an
/// abandoned connect must never rewrite the entry it started from.
fn cancel_prompt<T: 'static>(state: AppState, mut prompt: Signal<Option<T>>) {
    state.clear_pending_edit();
    prompt.set(None);
}

/// Inline secret prompt for a saved Postgres or SQL Server connection: the
/// database password, or the SSH key passphrase when the tunnel's key is
/// encrypted (Postgres only). "Remember" stores the secret in the OS keyring;
/// without it (or when no keyring is available) it lives in session memory
/// only.
#[component]
fn PasswordPromptCard(prompt: super::state::PasswordPrompt) -> Element {
    use super::state::PromptKind;
    let state = use_context::<AppState>();
    let mut password = use_signal(String::new);
    // Seeded from the prompt, not from `true`: a re-prompt after a rejected
    // passphrase carries the answer the user already gave (FRE-162).
    //
    // The seed only decides the box when this card was actually unmounted in
    // between — its `key:` is url + kind, which a re-prompt for the same
    // connection reuses. It is unmounted in practice, because the submit
    // clears `password_prompt` before the retry awaits. Both routes land on
    // the same value anyway: a surviving signal is holding the choice the user
    // just made, which is what the prompt now carries.
    let offered = prompt.remember;
    let mut remember = use_signal(move || offered);
    let prompt_for_submit = prompt.clone();
    let submit = move || {
        let prompt = prompt_for_submit.clone();
        let entered = password.peek().clone();
        let remember_choice = *remember.peek();
        // spawn_forever: the connect flow clears `password_prompt`, which
        // unmounts this card — a scope-tied `spawn` would be cancelled at
        // its next await, silently abandoning the connect.
        dioxus::core::spawn_forever(async move {
            // The prompt carries the backend the parked connect was for, so
            // the retry resumes on the same engine (FRE-139).
            match prompt.kind {
                PromptKind::DbPassword => {
                    state
                        .connect_server_with_password(
                            ServerBackend::of(prompt.backend),
                            prompt.url,
                            prompt.name,
                            entered,
                            remember_choice,
                            prompt.tunnel,
                        )
                        .await;
                }
                PromptKind::SshPassphrase => {
                    state
                        .connect_server_with_ssh_passphrase(prompt, entered, remember_choice)
                        .await;
                }
            }
        });
    };
    rsx! {
        div { class: "w-full max-w-xl rounded border border-cyan-300 dark:border-cyan-800 bg-slate-50 dark:bg-slate-950/80 p-4",
            p { class: "mb-2 text-sm text-slate-900 dark:text-slate-300",
                match prompt.kind {
                    PromptKind::DbPassword => "Password for ",
                    PromptKind::SshPassphrase => "SSH key passphrase for ",
                }
                span { class: "font-mono text-cyan-700 dark:text-cyan-300", "{prompt.name}" }
            }
            div { class: "flex gap-2",
                input {
                    r#type: "password",
                    class: "min-w-0 flex-1 rounded border border-slate-300 dark:border-slate-700 bg-slate-100 dark:bg-slate-950 px-3 py-2 font-mono text-sm text-slate-900 dark:text-slate-200",
                    autofocus: true,
                    value: "{password}",
                    oninput: move |evt| password.set(evt.value()),
                    onkeydown: {
                        let submit = submit.clone();
                        move |evt: KeyboardEvent| {
                            if evt.key() == Key::Enter {
                                submit();
                            }
                        }
                    },
                }
                button {
                    class: "rounded bg-cyan-700 px-4 py-2 text-sm font-medium text-white hover:bg-cyan-600",
                    onclick: move |_| submit(),
                    "Connect"
                }
                button {
                    class: "rounded px-3 py-2 text-sm text-slate-500 dark:text-slate-400 hover:text-slate-900 dark:hover:text-slate-100",
                    onclick: move |_| cancel_prompt(state, state.password_prompt),
                    "Cancel"
                }
            }
            label { class: "mt-2 flex items-center gap-2 text-xs text-slate-500 dark:text-slate-400",
                input {
                    r#type: "checkbox",
                    checked: remember(),
                    onchange: move |evt| remember.set(evt.checked()),
                }
                "Remember in the system keyring (falls back to this session only)"
            }
        }
    }
}

/// Trust-on-first-use prompt for an unrecognized SSH host key. Shows the
/// server's fingerprint so the user can compare it out-of-band; trusting
/// records the key in hubro's known_hosts store and retries the connect.
#[component]
fn HostKeyPromptCard(prompt: super::state::HostKeyPrompt) -> Element {
    let state = use_context::<AppState>();
    let prompt_for_trust = prompt.clone();
    let trust = move || {
        let prompt = prompt_for_trust.clone();
        // spawn_forever: trusting clears `host_key_prompt`, unmounting this
        // card — a scope-tied `spawn` would be cancelled mid-connect.
        dioxus::core::spawn_forever(async move {
            state.trust_host_and_connect(prompt).await;
        });
    };
    rsx! {
        div { class: "w-full max-w-xl rounded border border-amber-400 dark:border-amber-700 bg-amber-50 dark:bg-amber-950/40 p-4",
            p { class: "mb-1 text-sm font-medium text-amber-800 dark:text-amber-300",
                "Unrecognized SSH host key"
            }
            p { class: "mb-2 text-xs text-slate-600 dark:text-slate-400",
                "The server "
                span { class: "font-mono text-slate-900 dark:text-slate-200",
                    "{prompt.info.host}:{prompt.info.port}"
                }
                " is not in your known_hosts. Verify the fingerprint below matches the server before trusting it."
            }
            div { class: "mb-3 rounded bg-slate-100 dark:bg-slate-950 px-3 py-2 font-mono text-xs text-slate-800 dark:text-slate-200",
                div { "{prompt.info.key_type}" }
                div { class: "break-all", "{prompt.info.fingerprint}" }
            }
            div { class: "flex gap-2",
                button {
                    class: "rounded bg-amber-600 px-4 py-2 text-sm font-medium text-white hover:bg-amber-500",
                    onclick: move |_| trust(),
                    "Trust and connect"
                }
                button {
                    class: "rounded px-3 py-2 text-sm text-slate-500 dark:text-slate-400 hover:text-slate-900 dark:hover:text-slate-100",
                    onclick: move |_| cancel_prompt(state, state.host_key_prompt),
                    "Cancel"
                }
            }
        }
    }
}

/// Interactive Entra sign-in prompt (FRE-44). A Postgres or SQL Server connect
/// needs a Microsoft browser sign-in (no cached refresh token); the button
/// opens the browser and completes the connect, Cancel abandons it.
#[component]
fn EntraSignInCard(prompt: super::state::EntraPrompt) -> Element {
    let state = use_context::<AppState>();
    let prompt_for_signin = prompt.clone();
    let sign_in = move || {
        let prompt = prompt_for_signin.clone();
        // spawn_forever: signing in clears `entra_prompt`, unmounting this card;
        // a scope-tied spawn would be cancelled mid sign-in.
        dioxus::core::spawn_forever(async move {
            let backend = ServerBackend::of(prompt.backend);
            state
                .connect_server_with_entra_signin(backend, prompt)
                .await;
        });
    };
    rsx! {
        div { class: "w-full max-w-xl rounded border border-sky-300 dark:border-sky-800 bg-slate-50 dark:bg-slate-950/80 p-4",
            p { class: "mb-1 text-sm font-medium text-slate-900 dark:text-slate-200",
                "Sign in with Microsoft"
            }
            p { class: "mb-3 text-xs text-slate-600 dark:text-slate-400",
                "Connecting to "
                span { class: "font-mono text-cyan-700 dark:text-cyan-300", "{prompt.name}" }
                " opens your browser to sign in to Microsoft Entra ID."
            }
            div { class: "flex gap-2",
                button {
                    class: "rounded bg-sky-600 px-4 py-2 text-sm font-medium text-white hover:bg-sky-500",
                    onclick: move |_| sign_in(),
                    "Sign in with Microsoft"
                }
                button {
                    class: "rounded px-3 py-2 text-sm text-slate-500 dark:text-slate-400 hover:text-slate-900 dark:hover:text-slate-100",
                    onclick: move |_| cancel_prompt(state, state.entra_prompt),
                    "Cancel"
                }
            }
        }
    }
}

/// The signals every connection form holds, handed to [`submit_server_form`]
/// so one pipeline can read the fields at submit time (signals are `Copy`).
///
/// Deliberately only the *shared* fields: each form keeps its own option
/// signals (`sslmode` vs `encrypt` + trust-server-certificate) and folds them
/// into the URL it passes in, which is the one genuinely per-backend step of
/// the submit.
#[derive(Clone, Copy)]
struct ServerFormState {
    name: Signal<String>,
    /// Never prefilled; empty means "keep whatever the keyring holds"
    /// (FRE-75).
    password: Signal<String>,
    remember: Signal<bool>,
    use_url: Signal<bool>,
    pasted_url: Signal<String>,
    auth_mode: Signal<String>,
    entra_tenant: Signal<String>,
    entra_client_id: Signal<String>,
    use_tunnel: Signal<bool>,
    ssh_host: Signal<String>,
    ssh_port: Signal<String>,
    ssh_user: Signal<String>,
    ssh_use_key: Signal<bool>,
    ssh_key_path: Signal<String>,
    ssh_passphrase: Signal<String>,
    form_error: Signal<Option<String>>,
}

/// The submit pipeline both connection forms run (FRE-139): validate the
/// tunnel fields, take the SSH passphrase and any password embedded in a
/// pasted URL, settle the display name and auth mode, register the pending
/// edit, carry a moved secret across, then dispatch the connect and close (and
/// persist) only if it worked.
///
/// `built` is the URL the calling form's own fields produced — `Ok` or the
/// error to show. It is computed by the caller because that is the step the
/// two forms genuinely differ in (which `build_*`/`normalize_*` applies, and
/// SQL Server's trust-server-certificate flag); building it is side-effect
/// free, so a tunnel-field error still surfaces first, as before.
///
/// `old_locator` is set when editing a saved entry: the locator to replace,
/// which the edit may move.
fn submit_server_form(
    state: AppState,
    on_done: EventHandler<()>,
    backend: ServerBackend,
    form: ServerFormState,
    old_locator: Option<String>,
    built: Result<String, crate::db::DbError>,
) {
    use crate::tunnel::{TunnelAuth, TunnelConfig};
    let mut form_error = form.form_error;
    // Tunnel settings are validated first so a bad SSH field fails before any
    // connect attempt.
    let tunnel: Option<TunnelConfig> = match tunnel_from_form(
        *form.use_tunnel.peek(),
        &form.ssh_host.peek(),
        &form.ssh_port.peek(),
        &form.ssh_user.peek(),
        *form.ssh_use_key.peek(),
        &form.ssh_key_path.peek(),
    ) {
        Ok(tunnel) => tunnel,
        Err(err) => {
            form_error.set(Some(err));
            return;
        }
    };
    let entered_passphrase = if matches!(
        tunnel,
        Some(TunnelConfig {
            auth: TunnelAuth::KeyFile { .. },
            ..
        })
    ) {
        Some(form.ssh_passphrase.peek().clone()).filter(|p| !p.is_empty())
    } else {
        None
    };
    // A password pasted inside the URL is used for this connect (and
    // remembered for the session on success) but never persisted.
    let embedded_password = embedded_url_password(*form.use_url.peek(), &form.pasted_url.peek());
    let url = match built {
        Ok(url) => url,
        Err(err) => {
            form_error.set(Some(err.to_string()));
            return;
        }
    };
    let display_name = display_name_for(&form.name.peek(), &url);
    // Authentication mode from the selector (FRE-44/FRE-58). Entra takes the
    // `user` field as the Entra principal; a token replaces the password.
    let auth: ServerAuth = match auth_from_form(
        &form.auth_mode.peek(),
        &form.entra_tenant.peek(),
        &form.entra_client_id.peek(),
    ) {
        Ok(auth) => auth,
        Err(err) => {
            form_error.set(Some(err));
            return;
        }
    };
    if matches!(auth, ServerAuth::Entra(_)) && entra_principal_missing(&url) {
        form_error.set(Some(
            "Entra needs a principal — fill the user field (e.g. you@contoso.com)".to_string(),
        ));
        return;
    }
    let mut entered_password = form.password.peek().clone();
    if entered_password.is_empty() {
        if let Some(embedded) = embedded_password {
            entered_password = embedded;
        }
    }
    let remember_choice = *form.remember.peek();
    form_error.set(None);
    // spawn_forever: closing the modal (X or Escape) unmounts the form, and a
    // scope-tied `spawn` would be cancelled at its next await — abandoning the
    // connect with its reservation still held, which leaves the row stuck
    // showing progress it can no longer cancel.
    dioxus::core::spawn_forever(async move {
        // An edit is saved by whichever path confirms the connect — including
        // the Entra sign-in card, which resolves after this form has closed —
        // so the intent is registered up front rather than applied here
        // (FRE-75).
        if let Some(old) = &old_locator {
            state.set_pending_edit(old.clone(), url.clone());
        }
        // "Leave the password empty to keep the existing one" has to keep
        // working when the edit moves the locator: the stored secret is still
        // filed under the OLD one, and the connect below looks up the new.
        // Carry it over first (FRE-75).
        if entered_password.is_empty() {
            if let Some(old) = old_locator.as_ref().filter(|old| **old != url) {
                if let Ok(Some(stored)) = crate::secrets::get_password_async(old.clone()).await {
                    entered_password = stored;
                }
            }
        }
        // An entered passphrase seeds session memory so the tunnel open finds
        // it, exactly as if it came from the prompt.
        if let Some(passphrase) = &entered_passphrase {
            state.stash_ssh_passphrase(&url, passphrase.clone());
        }
        if matches!(auth, ServerAuth::Entra(_)) {
            // Entra: the connect either succeeds silently (and the flow saves
            // it) or raises the sign-in card (which saves on completion).
            // Either way, close the form — the card takes over.
            //
            // Closed *before* the await, not after: `on_done` is owned by the
            // connections screen, and a successful connect switches to the new
            // tab and unmounts it. This task outlives that (it is root-spawned),
            // so calling the handler afterwards would touch a dropped scope's
            // storage and panic. The Entra path yields after opening the tab
            // (it caches the refresh token), so the unmount really does land
            // first.
            //
            // Guarded because ordering alone is not a guarantee: a *second*
            // focus-taking connect finishing mid-submit can unmount the screen
            // at any await here. The screen is mounted exactly when this view
            // is active, and when it is not, closing the form is a no-op anyway
            // — it went with the screen.
            close_form(&state, &on_done);
            state
                .connect_server(
                    backend,
                    url.clone(),
                    display_name.clone(),
                    tunnel.clone(),
                    auth,
                )
                .await;
            return;
        }
        if entered_password.is_empty() {
            // No password entered: try as-is (the keyring may hold one); an
            // auth failure raises the password prompt.
            state
                .connect_server(
                    backend,
                    url.clone(),
                    display_name.clone(),
                    tunnel.clone(),
                    ServerAuth::Password,
                )
                .await;
        } else {
            state
                .connect_server_with_password(
                    backend,
                    url.clone(),
                    display_name.clone(),
                    entered_password,
                    remember_choice,
                    tunnel.clone(),
                )
                .await;
        }
        // The connect flow saves on success (`save_server_if_open`), which is
        // why nothing is saved here; only close the form when the connection
        // worked. Closing comes first for the reason above:
        // `persist_ssh_passphrase` awaits, and by the time it returns the
        // connections screen that owns `on_done` is gone.
        if state.open_locators.peek().iter().any(|(_, l)| *l == url) {
            close_form(&state, &on_done);
            if remember_choice && entered_passphrase.is_some() {
                state.persist_ssh_passphrase(&url).await;
            }
        }
    });
}

/// The password pasted inside a URL, when the form is in paste mode. Used for
/// that connect (and remembered for the session on success) but never
/// persisted — the saved locator is the password-free normalization.
fn embedded_url_password(use_url: bool, pasted: &str) -> Option<String> {
    if !use_url {
        return None;
    }
    url::Url::parse(pasted.trim()).ok().and_then(|parsed| {
        // Url::password() returns the still-encoded form. Decoded lossily,
        // unlike the field prefills: a password is used as typed, and refusing
        // to decode it would silently connect with the percent-escapes.
        parsed.password().map(|p| {
            percent_encoding::percent_decode_str(p)
                .decode_utf8_lossy()
                .into_owned()
        })
    })
}

/// The name a submitted connection is saved and tabbed under: what the user
/// typed, or the URL-derived fallback when they left the field empty.
fn display_name_for(entered: &str, url: &str) -> String {
    let entered = entered.trim();
    if entered.is_empty() {
        default_server_name(url)
    } else {
        entered.to_string()
    }
}

/// Add-Postgres panel: individual fields or a pasted URL, plus an optional
/// SSH tunnel.
#[component]
fn PostgresForm(
    /// The saved entry being edited (FRE-75); `None` is the add flow. Its
    /// locator is the key to replace on save — the edit may move it.
    edit: Option<SavedConnection>,
    on_done: EventHandler<()>,
) -> Element {
    let state = use_context::<AppState>();
    let prefill = edit.clone().map(EditPrefill::from_saved);
    let seed = prefill.clone().unwrap_or_default();
    let old_locator = edit.as_ref().map(|e| e.locator().to_string());
    let editing = edit.is_some();
    let mut host = use_signal(|| seed.host.clone());
    let mut port = use_signal(|| seed.port.clone());
    let mut database = use_signal(|| seed.database.clone());
    let mut user = use_signal(|| seed.user.clone());
    let mut sslmode = use_signal(|| seed.option.clone().unwrap_or_else(|| "prefer".to_string()));
    let form = ServerFormState {
        name: use_signal(|| seed.name.clone()),
        // Never prefilled: the stored secret isn't shown, and an empty field
        // on save means "keep the existing keyring entry" (FRE-75).
        password: use_signal(String::new),
        remember: use_signal(|| true),
        use_url: use_signal(|| false),
        pasted_url: use_signal(String::new),
        // Authentication: "password" (default), "entra-interactive", or
        // "entra-mi".
        auth_mode: use_signal(|| seed.auth_mode.clone()),
        entra_tenant: use_signal(|| seed.entra_tenant.clone()),
        entra_client_id: use_signal(|| seed.entra_client_id.clone()),
        use_tunnel: use_signal(|| seed.tunnel.is_some()),
        ssh_host: use_signal(|| seed.ssh_host.clone()),
        ssh_port: use_signal(|| seed.ssh_port.clone()),
        ssh_user: use_signal(|| seed.ssh_user.clone()),
        // false = ssh-agent (the default), true = key file.
        ssh_use_key: use_signal(|| seed.ssh_use_key),
        ssh_key_path: use_signal(|| seed.ssh_key_path.clone()),
        ssh_passphrase: use_signal(String::new),
        form_error: use_signal(|| Option::<String>::None),
    };
    let ServerFormState {
        mut name,
        mut password,
        mut remember,
        mut use_url,
        mut pasted_url,
        auth_mode,
        entra_tenant,
        entra_client_id,
        use_tunnel,
        ssh_host,
        ssh_port,
        ssh_user,
        ssh_use_key,
        ssh_key_path,
        ssh_passphrase,
        form_error,
    } = form;

    let submit = move || {
        // The URL is this form's own step; everything else is the shared
        // pipeline (FRE-139).
        let built = if *use_url.peek() {
            crate::db::normalize_pg_url(&pasted_url.peek())
        } else {
            crate::db::build_url(
                &host.peek(),
                &port.peek(),
                &database.peek(),
                &user.peek(),
                &sslmode.peek(),
            )
        };
        // Cloned per attempt: the submit takes ownership, and `submit` has to
        // stay FnMut.
        submit_server_form(
            state,
            on_done,
            ServerBackend::POSTGRES,
            form,
            old_locator.clone(),
            built,
        );
    };

    let field_class = FORM_FIELD_CLASS;
    rsx! {
        // Opaque, and a step above the page's slate-950: this renders over the
        // modal backdrop, and at 80% the connections list showed through it.
        div { class: "w-full max-w-xl rounded border border-slate-300 dark:border-slate-700 bg-slate-50 dark:bg-slate-900 p-4",
            div { class: "mb-3 flex items-center justify-between",
                span { class: "text-sm font-medium text-slate-900 dark:text-slate-200",
                    if editing { "Edit a Postgres connection" } else { "Add a Postgres connection" }
                }
                button {
                    class: "text-xs text-slate-500 dark:text-slate-400 hover:text-slate-900 dark:hover:text-slate-100",
                    onclick: move |_| {
                        let flip = !*use_url.read();
                        use_url.set(flip);
                    },
                    if use_url() { "Use individual fields" } else { "Paste a URL instead" }
                }
            }
            div { class: "flex flex-col gap-2",
                input {
                    class: field_class,
                    placeholder: "display name (optional)",
                    value: "{name}",
                    oninput: move |evt| name.set(evt.value()),
                }
                if use_url() {
                    input {
                        class: field_class,
                        placeholder: "postgres://user@host:5432/database?sslmode=require",
                        value: "{pasted_url}",
                        oninput: move |evt| pasted_url.set(evt.value()),
                    }
                } else {
                    div { class: "flex gap-2",
                        input {
                            class: "{field_class} flex-[3]",
                            placeholder: "host",
                            value: "{host}",
                            oninput: move |evt| host.set(evt.value()),
                        }
                        input {
                            class: "{field_class} flex-1",
                            placeholder: "5432",
                            value: "{port}",
                            oninput: move |evt| port.set(evt.value()),
                        }
                    }
                    div { class: "flex gap-2",
                        input {
                            class: field_class,
                            placeholder: "database",
                            value: "{database}",
                            oninput: move |evt| database.set(evt.value()),
                        }
                        input {
                            class: field_class,
                            placeholder: "user",
                            value: "{user}",
                            oninput: move |evt| user.set(evt.value()),
                        }
                    }
                    div { class: "flex gap-2",
                        select {
                            class: FORM_SELECT_CLASS,
                            onchange: move |evt| sslmode.set(evt.value()),
                            option { value: "prefer", selected: *sslmode.read() == "prefer", "sslmode: prefer" }
                            option { value: "require", selected: *sslmode.read() == "require", "sslmode: require" }
                            option { value: "disable", selected: *sslmode.read() == "disable", "sslmode: disable" }
                        }
                    }
                }
                // Authentication mode (FRE-44). Entra uses the user field as the
                // principal and a token instead of a password.
                AuthModeFields { auth_mode, entra_tenant, entra_client_id }
                if auth_mode() == "password" {
                    input {
                        r#type: "password",
                        class: field_class,
                        placeholder: if editing { "password (unchanged if left empty)" } else { "password" },
                        value: "{password}",
                        oninput: move |evt| password.set(evt.value()),
                    }
                }
                SshTunnelFields {
                    use_tunnel,
                    ssh_host,
                    ssh_port,
                    ssh_user,
                    ssh_use_key,
                    ssh_key_path,
                    ssh_passphrase,
                    radio_group: "pg-ssh-auth",
                }
                // Only meaningful for password auth — Entra caches its refresh
                // token in the keyring regardless.
                if auth_mode() == "password" {
                    label { class: "flex items-center gap-2 text-xs text-slate-500 dark:text-slate-400",
                        input {
                            r#type: "checkbox",
                            checked: remember(),
                            onchange: move |evt| remember.set(evt.checked()),
                        }
                        "Remember in the system keyring (falls back to this session only)"
                    }
                }
                div { class: "flex justify-end gap-2",
                    button {
                        class: "rounded px-3 py-2 text-sm text-slate-500 dark:text-slate-400 hover:text-slate-900 dark:hover:text-slate-100",
                        onclick: move |_| on_done.call(()),
                        "Cancel"
                    }
                    button {
                        class: "rounded bg-cyan-700 px-4 py-2 text-sm font-medium text-white hover:bg-cyan-600",
                        onclick: move |_| submit(),
                        if editing { "Save & connect" } else { "Connect & save" }
                    }
                }
                if let Some(err) = form_error() {
                    p { class: "text-sm text-red-600 dark:text-red-400", "{err}" }
                }
            }
        }
    }
}

/// Add-SQL-Server panel (FRE-57): individual fields or a pasted `mssql://`
/// URL, plus the Auth dropdown (password / Entra ID) and optional SSH tunnel
/// (both FRE-58). Deliberately close to [`PostgresForm`] but kept separate —
/// the two share the field styling, the auth/tunnel fieldsets
/// ([`AuthModeFields`] / [`SshTunnelFields`] and their validation helpers),
/// and the display-name fallback, while the option sets (encrypt vs sslmode,
/// trust-server-cert) differ enough that one shared form wasn't worth it.
#[component]
fn SqlServerForm(
    /// The saved entry being edited (FRE-75); `None` is the add flow.
    edit: Option<SavedConnection>,
    on_done: EventHandler<()>,
) -> Element {
    let state = use_context::<AppState>();
    let seed = edit
        .clone()
        .map(EditPrefill::from_saved)
        .unwrap_or_default();
    let old_locator = edit.as_ref().map(|e| e.locator().to_string());
    let editing = edit.is_some();
    let mut host = use_signal(|| seed.host.clone());
    let mut port = use_signal(|| seed.port.clone());
    let mut database = use_signal(|| seed.database.clone());
    let mut user = use_signal(|| seed.user.clone());
    // Matches the URL params the backend accepts: encrypt=on|off|plaintext.
    let mut encrypt = use_signal(|| seed.option.clone().unwrap_or_else(|| "on".to_string()));
    // Accept the server's TLS certificate without CA validation — needed for
    // dev servers with self-signed certs (e.g. the stock Docker image).
    let mut trust_cert = use_signal(|| seed.trust_cert);
    let form = ServerFormState {
        name: use_signal(|| seed.name.clone()),
        // Never prefilled: an empty field on save keeps the keyring entry.
        password: use_signal(String::new),
        remember: use_signal(|| true),
        use_url: use_signal(|| false),
        pasted_url: use_signal(String::new),
        // Authentication: "password" (default), "entra-interactive", or
        // "entra-mi".
        auth_mode: use_signal(|| seed.auth_mode.clone()),
        entra_tenant: use_signal(|| seed.entra_tenant.clone()),
        entra_client_id: use_signal(|| seed.entra_client_id.clone()),
        use_tunnel: use_signal(|| seed.tunnel.is_some()),
        ssh_host: use_signal(|| seed.ssh_host.clone()),
        ssh_port: use_signal(|| seed.ssh_port.clone()),
        ssh_user: use_signal(|| seed.ssh_user.clone()),
        // false = ssh-agent (the default), true = key file.
        ssh_use_key: use_signal(|| seed.ssh_use_key),
        ssh_key_path: use_signal(|| seed.ssh_key_path.clone()),
        ssh_passphrase: use_signal(String::new),
        form_error: use_signal(|| Option::<String>::None),
    };
    let ServerFormState {
        mut name,
        mut password,
        mut remember,
        mut use_url,
        mut pasted_url,
        auth_mode,
        entra_tenant,
        entra_client_id,
        use_tunnel,
        ssh_host,
        ssh_port,
        ssh_user,
        ssh_use_key,
        ssh_key_path,
        ssh_passphrase,
        form_error,
    } = form;

    let submit = move || {
        // The URL is this form's own step — the encrypt option and the
        // trust-server-certificate flag are SQL Server's alone; everything
        // else is the shared pipeline (FRE-139).
        let built = if *use_url.peek() {
            crate::db::normalize_mssql_url(&pasted_url.peek())
        } else {
            crate::db::build_mssql_url(
                &host.peek(),
                &port.peek(),
                &database.peek(),
                &user.peek(),
                &encrypt.peek(),
            )
            .map(|built| {
                if *trust_cert.peek() {
                    // build_mssql_url writes only the encrypt param; splice the
                    // trust flag in after it. The URL it returned always parses.
                    match url::Url::parse(&built) {
                        Ok(mut parsed) => {
                            parsed
                                .query_pairs_mut()
                                .append_pair("trustServerCertificate", "true");
                            String::from(parsed)
                        }
                        Err(_) => built,
                    }
                } else {
                    built
                }
            })
        };
        // Cloned per attempt: the submit takes ownership, and `submit` has to
        // stay FnMut.
        submit_server_form(
            state,
            on_done,
            ServerBackend::SQL_SERVER,
            form,
            old_locator.clone(),
            built,
        );
    };

    let field_class = FORM_FIELD_CLASS;
    rsx! {
        // Opaque over the modal backdrop, matching the Postgres form.
        div { class: "w-full max-w-xl rounded border border-slate-300 dark:border-slate-700 bg-slate-50 dark:bg-slate-900 p-4",
            div { class: "mb-3 flex items-center justify-between",
                span { class: "text-sm font-medium text-slate-900 dark:text-slate-200",
                    if editing { "Edit a SQL Server connection" } else { "Add a SQL Server connection" }
                }
                button {
                    class: "text-xs text-slate-500 dark:text-slate-400 hover:text-slate-900 dark:hover:text-slate-100",
                    onclick: move |_| {
                        let flip = !*use_url.read();
                        use_url.set(flip);
                    },
                    if use_url() { "Use individual fields" } else { "Paste a URL instead" }
                }
            }
            div { class: "flex flex-col gap-2",
                input {
                    class: field_class,
                    placeholder: "display name (optional)",
                    value: "{name}",
                    oninput: move |evt| name.set(evt.value()),
                }
                if use_url() {
                    input {
                        class: field_class,
                        placeholder: "mssql://user@host:1433/database?encrypt=on",
                        value: "{pasted_url}",
                        oninput: move |evt| pasted_url.set(evt.value()),
                    }
                } else {
                    div { class: "flex gap-2",
                        input {
                            class: "{field_class} flex-[3]",
                            placeholder: "host",
                            value: "{host}",
                            oninput: move |evt| host.set(evt.value()),
                        }
                        input {
                            class: "{field_class} flex-1",
                            placeholder: "1433",
                            value: "{port}",
                            oninput: move |evt| port.set(evt.value()),
                        }
                    }
                    div { class: "flex gap-2",
                        input {
                            class: field_class,
                            placeholder: "database",
                            value: "{database}",
                            oninput: move |evt| database.set(evt.value()),
                        }
                        input {
                            class: field_class,
                            placeholder: "user",
                            value: "{user}",
                            oninput: move |evt| user.set(evt.value()),
                        }
                    }
                    div { class: "flex gap-2",
                        select {
                            class: FORM_SELECT_CLASS,
                            onchange: move |evt| encrypt.set(evt.value()),
                            option { value: "on", selected: *encrypt.read() == "on", "encrypt: on" }
                            option { value: "off", selected: *encrypt.read() == "off", "encrypt: off (login only)" }
                            option { value: "plaintext", selected: *encrypt.read() == "plaintext", "encrypt: plaintext" }
                        }
                    }
                    label { class: "flex items-center gap-2 text-xs text-slate-500 dark:text-slate-400",
                        input {
                            r#type: "checkbox",
                            checked: trust_cert(),
                            onchange: move |evt| trust_cert.set(evt.checked()),
                        }
                        "Trust the server certificate (self-signed / dev servers)"
                    }
                }
                // Authentication mode (FRE-58). Entra uses the user field as
                // the principal and a token instead of a password.
                AuthModeFields { auth_mode, entra_tenant, entra_client_id }
                if auth_mode() == "password" {
                    input {
                        r#type: "password",
                        class: field_class,
                        placeholder: if editing { "password (unchanged if left empty)" } else { "password" },
                        value: "{password}",
                        oninput: move |evt| password.set(evt.value()),
                    }
                }
                SshTunnelFields {
                    use_tunnel,
                    ssh_host,
                    ssh_port,
                    ssh_user,
                    ssh_use_key,
                    ssh_key_path,
                    ssh_passphrase,
                    radio_group: "mssql-ssh-auth",
                }
                // Only meaningful for password auth — Entra caches its refresh
                // token in the keyring regardless.
                if auth_mode() == "password" {
                    label { class: "flex items-center gap-2 text-xs text-slate-500 dark:text-slate-400",
                        input {
                            r#type: "checkbox",
                            checked: remember(),
                            onchange: move |evt| remember.set(evt.checked()),
                        }
                        "Remember in the system keyring (falls back to this session only)"
                    }
                }
                div { class: "flex justify-end gap-2",
                    button {
                        class: "rounded px-3 py-2 text-sm text-slate-500 dark:text-slate-400 hover:text-slate-900 dark:hover:text-slate-100",
                        onclick: move |_| on_done.call(()),
                        "Cancel"
                    }
                    button {
                        class: "rounded bg-red-800 px-4 py-2 text-sm font-medium text-white hover:bg-red-700",
                        onclick: move |_| submit(),
                        if editing { "Save & connect" } else { "Connect & save" }
                    }
                }
                if let Some(err) = form_error() {
                    p { class: "text-sm text-red-600 dark:text-red-400", "{err}" }
                }
            }
        }
    }
}

/// Closes a connection form from inside its submit task.
///
/// The task is root-spawned so closing the modal cannot abandon the connect,
/// which means it outlives the form — and `on_done` is a [`Callback`] owned by
/// the connections screen's scope, whose storage is freed when that scope is
/// dropped. Calling it then panics rather than no-oping (dioxus has no
/// `Callback::try_call`), and any connect that opens a focused tab drops the
/// screen. Calls are ordered to land before that happens; this is the backstop
/// for the case ordering cannot cover, a *sibling* connect finishing mid-submit.
///
/// `ActiveView::Connections` holds exactly while the screen is mounted, and
/// when it does not, the form is already gone with it.
fn close_form(state: &AppState, on_done: &EventHandler<()>) {
    if matches!(*state.active.peek(), ActiveView::Connections) {
        on_done.call(());
    }
}

/// Text-input class shared by the Postgres and SQL Server connection forms.
const FORM_FIELD_CLASS: &str ="w-full rounded border border-slate-300 dark:border-slate-700 bg-slate-100 dark:bg-slate-950 px-3 py-2 font-mono text-sm text-slate-900 dark:text-slate-200 placeholder:text-slate-400 dark:placeholder:text-slate-600";

/// Select class shared by the connection forms' dropdowns.
const FORM_SELECT_CLASS: &str = "rounded border border-slate-300 dark:border-slate-700 bg-slate-100 dark:bg-slate-950 px-2 py-2 text-sm text-slate-900 dark:text-slate-200";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_prompt_card_offers_the_choice_the_prompt_carries() {
        // The last hop of FRE-162, and the one that can silently undo all of
        // it: `withdraw_ssh_remember` can compute the right answer and
        // `PasswordPrompt` can carry it faithfully while the card initialises
        // its checkbox from a literal `true` — which is exactly the code this
        // replaced. Every state-side test stays green through that.
        //
        // A component needs a Dioxus runtime to render, so the seed is checked
        // at its source: the card is a function, and the initialiser is one
        // line of it.
        let source = include_str!("connections.rs");
        let card = source
            .find("fn PasswordPromptCard(")
            .expect("the prompt card must still exist");
        let body = &source[card..card
            + source[card..]
                .find("rsx!")
                .expect("the card must still render")];
        assert!(
            !body.contains("use_signal(|| true)"),
            "the card re-ticks the remember box on every prompt, so unticking \
             it and then mistyping the passphrase silently restores the \
             decision to store it (FRE-162): {body}"
        );
        // Polarity, not merely the mention: `let offered = !prompt.remember;`
        // reads as correctly wired up while offering every user the opposite
        // of what they chose.
        assert!(
            body.contains("let offered = prompt.remember;")
                && body.contains("use_signal(move || offered)"),
            "the card ignores the choice the prompt carries, or inverts it: {body}"
        );
        // The other end of the same value, which this slice already covers:
        // what the box holds when the card submits. Older than FRE-162 and
        // unchanged by it, but it is the same one-line inversion at the same
        // seam — and `!*remember.peek()` would send every user the opposite of
        // the box they ticked, on the read this test is already looking at.
        assert!(
            body.contains("let remember_choice = *remember.peek();"),
            "the card submits something other than the box as ticked, so the \
             answer the user gave is not the one that reaches the keyring \
             decision: {body}"
        );
        // And the third place the same value passes through: what the rendered
        // box displays. Rendering is not testable without a runtime, but the
        // *binding* is text like the other two, and `checked: !remember()`
        // shows every user the opposite of the state they are about to submit.
        // Sliced from `rsx!` to the end of the function, so all three ends of
        // the value — seed, display, submit — are covered by one test.
        let rendered = &source[card + body.len()
            ..card
                + source[card..]
                    .find("\n}\n")
                    .expect("the card function must be closed")];
        assert!(
            rendered.contains("checked: remember(),"),
            "the checkbox displays something other than the choice it holds, \
             so the box the user sees and the answer the card submits can \
             disagree: {rendered}"
        );
    }

    #[test]
    fn an_embedded_password_is_only_taken_from_a_pasted_url() {
        // Field mode: the password field is the only source, even if the
        // (hidden) paste field still holds an old value.
        assert_eq!(
            embedded_url_password(false, "postgres://u:secret@h:5432/db"),
            None
        );
        assert_eq!(
            embedded_url_password(true, "postgres://u:secret@h:5432/db"),
            Some("secret".to_string())
        );
        // Percent escapes are what the user typed, so they are decoded before
        // the password is used.
        assert_eq!(
            embedded_url_password(true, "mssql://sa:p%40ss%20w%25rd@h:1433/db"),
            Some("p@ss w%rd".to_string())
        );
        // Surrounding whitespace is trimmed like the URL builders do.
        assert_eq!(
            embedded_url_password(true, "  postgres://u:secret@h/db  "),
            Some("secret".to_string())
        );
        // Nothing to take: no password in the URL, or nothing parseable.
        assert_eq!(embedded_url_password(true, "postgres://u@h:5432/db"), None);
        assert_eq!(embedded_url_password(true, "not a url"), None);
        assert_eq!(embedded_url_password(true, ""), None);
    }

    #[test]
    fn a_search_expands_a_folded_group_so_its_hits_are_never_a_bare_header() {
        let collapsed = vec!["Production".to_string()];
        // Unsearched, the fold holds.
        assert!(section_collapsed(Some("Production"), &collapsed, false));
        assert!(!section_collapsed(Some("Archive"), &collapsed, false));
        // Searching, it does not: `arrange` only returns sections that
        // matched, so a folded one would be a header with a count and no rows
        // — and the row behind it is the one being searched for.
        assert!(!section_collapsed(Some("Production"), &collapsed, true));
        // The override doesn't clear the fold, so it comes back when the
        // search does.
        assert!(section_collapsed(Some("Production"), &collapsed, false));
        // The ungrouped section is not a group and has no fold.
        assert!(!section_collapsed(None, &collapsed, false));
        assert!(!section_collapsed(None, &collapsed, true));
    }

    #[test]
    fn an_empty_name_field_falls_back_to_the_url() {
        assert_eq!(
            display_name_for("  ", "postgres://u@db.example.com:5432/app"),
            "app @ db.example.com"
        );
        assert_eq!(
            display_name_for("", "mssql://sa@db.example.com:1433/app"),
            "app @ db.example.com"
        );
        // A typed name wins, trimmed.
        assert_eq!(
            display_name_for("  prod  ", "postgres://u@db.example.com:5432/app"),
            "prod"
        );
    }
}

/// Builds the tunnel config from the SSH form fields — `Ok(None)` when the
/// toggle is off. Shared by the Postgres and SQL Server forms; errors are
/// user-facing form errors, raised before any connect attempt.
fn tunnel_from_form(
    use_tunnel: bool,
    host: &str,
    port: &str,
    user: &str,
    use_key: bool,
    key_path: &str,
) -> Result<Option<crate::tunnel::TunnelConfig>, String> {
    use crate::tunnel::{TunnelAuth, TunnelConfig};
    if !use_tunnel {
        return Ok(None);
    }
    let host = host.trim().to_string();
    if host.is_empty() {
        return Err("SSH host must not be empty".to_string());
    }
    let port_text = port.trim();
    let port = if port_text.is_empty() {
        22
    } else {
        match port_text.parse::<u16>() {
            // 0 parses as a valid u16 but is not a usable port.
            Ok(0) | Err(_) => return Err(format!("invalid SSH port: {port_text}")),
            Ok(port) => port,
        }
    };
    let user = user.trim().to_string();
    if user.is_empty() {
        return Err("SSH user must not be empty".to_string());
    }
    let auth = if use_key {
        let path = key_path.trim().to_string();
        if path.is_empty() {
            return Err("SSH key file path must not be empty".to_string());
        }
        // The placeholder suggests ~/.ssh/…, so honor a leading ~/.
        let path = match path.strip_prefix("~/") {
            Some(rest) => match dirs::home_dir() {
                Some(home) => home.join(rest),
                None => PathBuf::from(path),
            },
            None => PathBuf::from(path),
        };
        TunnelAuth::KeyFile { path }
    } else {
        TunnelAuth::Agent
    };
    Ok(Some(TunnelConfig {
        host,
        port,
        user,
        auth,
    }))
}

/// Builds the auth mode from the form's Auth selector ("password",
/// "entra-interactive", or "entra-mi") and Entra fields — the FRE-44/FRE-49
/// validation shared by both server forms.
fn auth_from_form(mode: &str, tenant: &str, client_id: &str) -> Result<ServerAuth, String> {
    use crate::azure::EntraAuth;
    let client = client_id.trim().to_string();
    match mode {
        "entra-interactive" => {
            let tenant = tenant.trim().to_string();
            if tenant.is_empty() {
                return Err("the Entra tenant must not be empty".to_string());
            }
            Ok(ServerAuth::Entra(EntraAuth::Interactive {
                tenant,
                client_id: (!client.is_empty()).then_some(client),
            }))
        }
        "entra-mi" => Ok(ServerAuth::Entra(EntraAuth::ManagedIdentity {
            client_id: (!client.is_empty()).then_some(client),
        })),
        _ => Ok(ServerAuth::Password),
    }
}

/// FRE-49 validation shared by both server forms: Entra needs the URL's
/// username as the principal — an empty one would fail later at the server
/// with an opaque error.
fn entra_principal_missing(url: &str) -> bool {
    url::Url::parse(url)
        .ok()
        .is_none_or(|u| u.username().is_empty())
}

/// The Auth dropdown (password / Entra interactive / Entra managed identity)
/// plus the Entra fields it reveals — shared by the Postgres and SQL Server
/// forms, which hand their own signals in (signals are `Copy`).
#[component]
fn AuthModeFields(
    auth_mode: Signal<String>,
    entra_tenant: Signal<String>,
    entra_client_id: Signal<String>,
) -> Element {
    let field_class = FORM_FIELD_CLASS;
    rsx! {
        select {
            class: FORM_SELECT_CLASS,
            onchange: move |evt| auth_mode.set(evt.value()),
            option { value: "password", selected: auth_mode() == "password", "Auth: password" }
            option {
                value: "entra-interactive",
                selected: auth_mode() == "entra-interactive",
                "Auth: Microsoft Entra ID (browser sign-in)"
            }
            option {
                value: "entra-mi",
                selected: auth_mode() == "entra-mi",
                "Auth: Microsoft Entra ID (managed identity)"
            }
        }
        if auth_mode() == "entra-interactive" {
            input {
                class: field_class,
                placeholder: "Entra tenant — id, domain, or 'organizations'",
                value: "{entra_tenant}",
                oninput: move |evt| entra_tenant.set(evt.value()),
            }
        }
        if auth_mode().starts_with("entra") {
            input {
                class: field_class,
                placeholder: "application (client) ID — optional",
                value: "{entra_client_id}",
                oninput: move |evt| entra_client_id.set(evt.value()),
            }
            p { class: "text-xs text-slate-500 dark:text-slate-400",
                "The user field is your Entra principal (e.g. you@contoso.com); a token is used instead of a password."
            }
        }
    }
}

/// The "Connect through an SSH tunnel" toggle and its fieldset — shared by the
/// Postgres and SQL Server forms. `radio_group` keeps the two forms' auth
/// radios in separate groups.
#[component]
fn SshTunnelFields(
    use_tunnel: Signal<bool>,
    ssh_host: Signal<String>,
    ssh_port: Signal<String>,
    ssh_user: Signal<String>,
    ssh_use_key: Signal<bool>,
    ssh_key_path: Signal<String>,
    ssh_passphrase: Signal<String>,
    radio_group: String,
) -> Element {
    let field_class = FORM_FIELD_CLASS;
    rsx! {
        label { class: "flex items-center gap-2 text-xs text-slate-500 dark:text-slate-400",
            input {
                r#type: "checkbox",
                checked: use_tunnel(),
                onchange: move |evt| use_tunnel.set(evt.checked()),
            }
            "Connect through an SSH tunnel"
        }
        if use_tunnel() {
            div { class: "flex flex-col gap-2 rounded border border-slate-200 dark:border-slate-800 bg-slate-100 dark:bg-slate-900/60 p-3",
                div { class: "flex gap-2",
                    input {
                        class: "{field_class} flex-[3]",
                        placeholder: "ssh host",
                        value: "{ssh_host}",
                        oninput: move |evt| ssh_host.set(evt.value()),
                    }
                    input {
                        class: "{field_class} flex-1",
                        placeholder: "22",
                        value: "{ssh_port}",
                        oninput: move |evt| ssh_port.set(evt.value()),
                    }
                }
                input {
                    class: field_class,
                    placeholder: "ssh user",
                    value: "{ssh_user}",
                    oninput: move |evt| ssh_user.set(evt.value()),
                }
                div { class: "flex gap-4 text-xs text-slate-500 dark:text-slate-400",
                    label { class: "flex items-center gap-2",
                        input {
                            r#type: "radio",
                            name: "{radio_group}",
                            checked: !ssh_use_key(),
                            onchange: move |_| ssh_use_key.set(false),
                        }
                        "ssh-agent"
                    }
                    label { class: "flex items-center gap-2",
                        input {
                            r#type: "radio",
                            name: "{radio_group}",
                            checked: ssh_use_key(),
                            onchange: move |_| ssh_use_key.set(true),
                        }
                        "key file"
                    }
                }
                if ssh_use_key() {
                    input {
                        class: field_class,
                        placeholder: "key file path, e.g. ~/.ssh/id_ed25519",
                        value: "{ssh_key_path}",
                        oninput: move |evt| ssh_key_path.set(evt.value()),
                    }
                    input {
                        r#type: "password",
                        class: field_class,
                        placeholder: "key passphrase (if the key is encrypted)",
                        value: "{ssh_passphrase}",
                        oninput: move |evt| ssh_passphrase.set(evt.value()),
                    }
                }
            }
        }
    }
}

/// Fallback display name for a server connection URL: "database @ host".
/// Scheme-agnostic, so the Postgres and SQL Server forms share it.
fn default_server_name(url: &str) -> String {
    match url::Url::parse(url) {
        Ok(parsed) => {
            let db = parsed.path().trim_start_matches('/');
            let host = parsed.host_str().unwrap_or("?");
            if db.is_empty() {
                host.to_string()
            } else {
                format!("{db} @ {host}")
            }
        }
        Err(_) => url.to_string(),
    }
}
