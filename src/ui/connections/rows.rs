//! One saved connection's row in the list, and the settings drawer that
//! opens under it: group, write protection and accent colour.

use dioxus::prelude::*;
use dioxus_icons::lucide::{LogOut, Pencil, ShieldAlert, Trash2, X};

use crate::config::{BackendKind, ConnectionColor};
use crate::db::{engine_has_read_only_mode, Dialect, WriteProtection};
use crate::ui::classes::{BTN_ICON, SELECT};
use crate::ui::icons::BackendIcon;
use crate::ui::notice::{ConfirmInline, Spinner};
use crate::ui::state::{AppState, TabFocus};

use super::{ConnectForm, SavedRow};

/// One saved connection's row. Lifted out of [`ConnectionsScreen`] when
/// grouping put a second loop around it: the screen now walks
/// sections of rows, and 200 lines of row markup inside that walk would have
/// buried it.
///
/// The screen's signals are passed in rather than re-derived. A `Signal` is
/// `Copy`, so this costs nothing, and it keeps "one row armed, one editor
/// open" a property of the screen rather than something each row has to
/// agree about.
#[component]
pub(super) fn SavedConnectionRow(
    row: SavedRow,
    confirm_remove: Signal<Option<String>>,
    settings_open: Signal<Option<String>>,
    open_form: Signal<Option<ConnectForm>>,
) -> Element {
    let state = use_context::<AppState>();
    let mut confirm_remove = confirm_remove;
    let mut settings_open = settings_open;
    let mut open_form = open_form;
    // Asked once per render, by borrow: three checks below need it, and each
    // used to clone the signal's string and the row's locator to compare.
    let settings_shown = settings_open.read().as_deref() == Some(row.locator.as_str());
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
            // Accent stripe: the colour warns, and
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
                                if evt.modifiers().shift() {
                                    TabFocus::Background
                                } else {
                                    TabFocus::Front
                                },
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
                    span { class: row.backend.badge_classes(), "{row.backend.label()}" }
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
                    // Protection is never silent:
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
                            class: BTN_ICON,
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
                    // confirmation, the same `ConfirmInline` as
                    // every other two-step destructive action.
                    ConfirmInline {
                        prompt: "Remove?".to_string(),
                        confirm_label: "Remove",
                        on_confirm: {
                            let locator = row.locator.clone();
                            move |_| {
                                state.remove_saved(&locator);
                                confirm_remove.set(None);
                            }
                        },
                        on_cancel: move |_| confirm_remove.set(None),
                    }
                } else {
                    // Marking is offered for every backend,
                    // including SQLite — which has no edit
                    // form, so this is the only place it can
                    // be protected from.
                    button {
                        class: if settings_shown {
                            "cursor-pointer rounded bg-slate-300 dark:bg-slate-700 p-1.5 text-slate-900 dark:text-slate-200"
                        } else {
                            BTN_ICON
                        },
                        title: "Group, write protection and colour",
                        aria_label: "Group, write protection and colour",
                        aria_expanded: if settings_shown { "true" } else { "false" },
                        onclick: {
                            let locator = row.locator.clone();
                            move |_| {
                                let open = settings_open.peek().as_deref() == Some(locator.as_str());
                                settings_open.set((!open).then(|| locator.clone()));
                            }
                        },
                        ShieldAlert { size: 14 }
                    }
                    if row.backend != BackendKind::Sqlite {
                        button {
                            class: BTN_ICON,
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
                    // Offered whether or not a sign-in is kept right now:
                    // knowing would take a keyring read per row on every
                    // render of this screen, and signing out of nothing is
                    // harmless. Only a connection that can keep one is
                    // offered it at all.
                    if row.auth.keeps_entra_sign_in() {
                        button {
                            class: BTN_ICON,
                            title: "Sign out: forget the remembered Microsoft sign-in",
                            aria_label: "Sign out of Microsoft Entra ID",
                            onclick: {
                                let (locator, name) = (row.locator.clone(), row.name.clone());
                                move |_| state.sign_out_entra(locator.clone(), name.clone())
                            },
                            LogOut { size: 14 }
                        }
                    }
                    button {
                        class: BTN_ICON,
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
            // The settings editor, expanded
            // inline below its row — the row is already a
            // full-width click target, so a floating popover
            // would have to be dismissed before the row could
            // be used.
            if settings_shown {
                RowSettings {
                    locator: row.locator.clone(),
                    backend: row.backend,
                    protection: row.protection,
                    color: row.color,
                    group: row.group.clone(),
                }
            }
        }
    }
}

/// One saved connection's per-row settings, edited inline under its row in
/// the connections list: which group it is filed under, and its
/// write protection and accent colour.
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
    /// Decides what the read-only hint can promise — see [`protection_hint`].
    backend: BackendKind,
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
                        class: SELECT,
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
                "{protection_hint(protection, backend)}"
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
///
/// The read-only sentence depends on the backend, because what enforces it
/// does: where the engine has a read-only mode ([`engine_has_read_only_mode`])
/// the SQL editor's scripts run in it, and the database refuses a write hubro
/// misread; SQL Server has none, so there hubro's reading of each statement is
/// all there is. Promising more than that would be worse than promising less.
fn protection_hint(protection: WriteProtection, backend: BackendKind) -> &'static str {
    match protection {
        WriteProtection::Open => "Writes run without extra confirmation.",
        WriteProtection::Confirm => {
            "Every write asks first, naming this connection. Colour only warns — it changes nothing."
        }
        WriteProtection::ReadOnly if !engine_has_read_only_mode(backend.into()) => {
            "Writes are refused, including from the SQL editor. SQL Server has no read-only \
             mode, so a write hidden in a function or procedure call is not caught — use a \
             login without write permission for that."
        }
        // The read-only transaction covers this connection only; a function
        // that writes over a connection of its own is beyond it.
        WriteProtection::ReadOnly if backend == BackendKind::Postgres => {
            "Writes are refused, including from the SQL editor and from browsing a view that \
             writes, where the database itself refuses them too — except through a function \
             that opens a connection of its own, such as dblink or Citus's run_command_on_*."
        }
        WriteProtection::ReadOnly => {
            "Writes are refused, including from the SQL editor, where the database itself \
             refuses them too."
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_read_only_hint_promises_the_database_refuses_only_where_it_does() {
        for backend in [
            BackendKind::Sqlite,
            BackendKind::Postgres,
            BackendKind::SqlServer,
        ] {
            let hint = protection_hint(WriteProtection::ReadOnly, backend);
            assert_eq!(
                hint.contains("database itself"),
                engine_has_read_only_mode(backend.into()),
                "{backend:?}: {hint}"
            );
            assert_eq!(
                hint.contains("not caught"),
                !engine_has_read_only_mode(backend.into()),
                "{backend:?}: {hint}"
            );
        }
        // Postgres's promise carries its exception: Citus writes through
        // `run_command_on_*` on a marked connection (`tests/db_read_only.rs`).
        let postgres = protection_hint(WriteProtection::ReadOnly, BackendKind::Postgres);
        assert!(postgres.contains("except"), "{postgres}");
        assert!(postgres.contains("run_command_on_"), "{postgres}");
    }
}
