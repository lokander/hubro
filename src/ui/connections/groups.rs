//! The connections list's groups: a group's header with its fold, rename,
//! reorder and delete, creating one, and when a section renders folded.

use dioxus::prelude::*;
use dioxus_icons::lucide::{ChevronDown, ChevronRight, ChevronUp, Pencil, Trash2};

use crate::config::GroupStep;
use crate::ui::classes::BTN_ICON;
use crate::ui::js::focus_on_mount;
use crate::ui::notice::ConfirmInline;
use crate::ui::state::AppState;
use crate::ui::text_field::TextInput;

/// Whether a section renders folded.
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
pub(super) fn section_collapsed(name: Option<&str>, collapsed: &[String], searching: bool) -> bool {
    match name {
        Some(name) if !searching => collapsed.iter().any(|folded| folded == name),
        _ => false,
    }
}

/// Creates the group named in the "New group" field, closing the field on
/// success and leaving it open (with the reason) on failure — a refused name
/// is still on screen to fix.
pub(super) fn submit_new_group(
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
/// that only make sense on a group — rename, reorder, delete.
///
/// Reordering is a pair of one-step buttons rather than drag-and-drop: a drag
/// implementation worth using is a dependency, and the thing being ordered is
/// a handful of names.
#[component]
pub(super) fn GroupHeader(
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
                TextInput {
                    class: "m-1.5 min-w-0 flex-1 rounded border border-slate-300 dark:border-slate-700 bg-slate-50 dark:bg-slate-950 px-2 py-1 text-sm text-slate-900 dark:text-slate-200",
                    text: "{rename_draft}",
                    onmounted: focus_on_mount,
                    oninput: move |evt: FormEvent| rename_draft.set(evt.value()),
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
                div { class: "flex-1 px-3 py-1.5",
                    ConfirmInline {
                        prompt: format!("Delete “{name}”? Its connections stay, ungrouped."),
                        confirm_label: "Delete",
                        on_confirm: {
                            let name = name.clone();
                            move |_| {
                                state.remove_saved_group(&name);
                                confirm_delete_group.set(None);
                            }
                        },
                        on_cancel: move |_| confirm_delete_group.set(None),
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
                    class: BTN_ICON,
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
                        BTN_ICON
                    },
                    disabled: first,
                    title: "Move group up",
                    aria_label: "Move group up",
                    onclick: {
                        let name = name.clone();
                        move |_| state.move_saved_group(&name, GroupStep::Up)
                    },
                    ChevronUp { size: 14 }
                }
                button {
                    class: if last {
                        "rounded p-1.5 text-slate-300 dark:text-slate-700"
                    } else {
                        BTN_ICON
                    },
                    disabled: last,
                    title: "Move group down",
                    aria_label: "Move group down",
                    onclick: {
                        let name = name.clone();
                        move |_| state.move_saved_group(&name, GroupStep::Down)
                    },
                    ChevronDown { size: 14 }
                }
                button {
                    class: BTN_ICON,
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

#[cfg(test)]
mod tests {
    use super::*;

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
}
