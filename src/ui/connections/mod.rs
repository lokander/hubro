//! The connections screen: the saved-connections list, the
//! add/edit connection forms behind their modal, and the cards that take over
//! when a connect parks on a prompt (password, SSH host key, Entra sign-in).
//!
//! Self-contained apart from [`AppState`]: nothing here is reachable from the
//! rest of the shell, which only renders [`ConnectionsScreen`] for
//! [`ActiveView::Connections`](super::state::ActiveView::Connections).
//!
//! The screen and the list it walks live here; the rest is split by what it
//! holds:
//!
//! - `rows`: one saved connection's row, and its settings drawer;
//! - `groups`: a group's header, creating one, and when a section folds;
//! - `prompts`: the password/passphrase, host-key and Entra sign-in cards;
//! - `forms`: the server connection form and its submit;
//! - `tunnel`: the form's SSH tunnel fields.
//!
//! The source pins over this module read every file in it
//! (`test_support::module_code`), so a function moving between these files
//! is not a failure, and a second instance of what a pin counts is found
//! whichever file it lands in.

mod forms;
mod groups;
mod prompts;
mod rows;
mod tunnel;

use dioxus::prelude::*;
use dioxus_icons::lucide::{FolderPlus, Plug, Search, X};

use crate::config::{BackendKind, ConnectionColor, SavedConnection, ServerAuth};
use crate::db::{Dialect, WriteProtection};
use crate::ui::classes::BTN_PRIMARY;

use super::icons::BackendIcon;
use super::js::focus_on_mount;
use super::notice::{Banner, BannerKind, EmptyState, Overlay, OverlayAlign};
use super::state::{AppState, ConnectStep};
use super::text_field::TextInput;

use forms::{ServerForm, ServerSpec};
use groups::{section_collapsed, submit_new_group, GroupHeader};
use prompts::{EntraSignInCard, HostKeyPromptCard, PasswordPromptCard};
use rows::SavedConnectionRow;

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
    /// The row's write protection and accent colour.
    protection: WriteProtection,
    color: Option<ConnectionColor>,
    /// The group this connection is filed under, if any.
    group: Option<String>,
    tunnel: Option<crate::tunnel::TunnelConfig>,
    auth: ServerAuth,
}

/// One section of the connections list as it renders: a group and
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

/// Which connection form the modal is showing. SQLite has no
/// form — it goes straight to the native file picker.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ConnectForm {
    Postgres,
    SqlServer,
    /// Editing an existing saved entry, prefilled from it. Carries
    /// the entry itself so the form knows which locator to replace — the
    /// edit may move it.
    ///
    /// Boxed because the entry dwarfs the other two variants, which carry
    /// nothing: unboxed, every `Option<ConnectForm>` in the screen would be
    /// the size of a whole saved connection.
    Edit(Box<SavedConnection>),
}

/// Modal shell for the add-connection forms: an [`Overlay`] that a backdrop
/// click does not dismiss, since these panels hold a half-typed connection —
/// only Escape or an explicit Cancel/✕ closes them.
///
/// Top-aligned ([`OverlayAlign::Top`], with the panel's `my-auto` margins
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
        Overlay {
            on_close,
            dismiss_on_backdrop: false,
            align: OverlayAlign::Top,
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

/// Launch screen: the persisted saved-connections list plus add flows for
/// SQLite (native file picker) and Postgres (form or URL).
#[component]
pub(super) fn ConnectionsScreen() -> Element {
    let mut state = use_context::<AppState>();
    // One open form at a time (they were a mutually-exclusive bool pair
    // before the forms moved into a modal); `None` means no modal is up.
    let mut open_form = use_signal(|| Option::<ConnectForm>::None);
    // Locator of the row whose Remove is armed, if any. Removing is instant
    // and unrecoverable (a server entry takes its keyring password with it),
    // so the trash icon only arms the confirmation — one row at a time, since
    // arming another replaces this.
    let confirm_remove = use_signal(|| Option::<String>::None);
    // Which row's settings editor is expanded: group, write
    // protection and colour.
    let settings_open = use_signal(|| Option::<String>::None);
    // The connections search. Local to this screen, like the
    // sidebar's filter: it narrows a view rather than changing anything, so
    // it is not persisted and the screen opens unfiltered.
    let mut search = use_signal(String::new);
    // The "New group" field, shown only while it is being typed into.
    let mut naming_group = use_signal(|| false);
    let mut new_group = use_signal(String::new);
    // Which group's name is being edited, and the draft.
    let renaming_group = use_signal(|| Option::<String>::None);
    let rename_draft = use_signal(String::new);
    // Which group's Delete is armed — same one-at-a-time arming as a row's.
    let confirm_delete_group = use_signal(|| Option::<String>::None);
    // Why the last group name was refused, shown under the field it was
    // typed into.
    let mut group_error = use_signal(|| Option::<String>::None);
    let error = state.connect_error.read().clone();
    let notice = state.connect_notice.read().clone();
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
    let has_groups = !state.saved.read().groups().is_empty();
    // Whether anything is saved at all — the empty state's condition, which
    // a search that matches nothing must not trigger.
    let has_saved = !state.saved.read().entries().is_empty();
    // The list as it renders: each group in its configured order
    // with the connections filed under it, then the ungrouped ones, narrowed
    // by the search box. `arrange` owns those two rules so the render body
    // stays a straight walk over what it returned.
    //
    // Memoized, not built in the render body. Building it is not free:
    // `arrange` clones every saved entry, and each SQLite row's open-locator
    // canonicalizes its path — a filesystem call per row. And this screen
    // re-renders for much that is not the list: each keystroke of a new
    // group's name, a prompt card appearing, a connect error. Reads exactly
    // the six signals the list is made of, so it rebuilds when one of those
    // changes and otherwise hands the render the list it already has.
    let sections = use_memo(move || {
        let query = search();
        // The same test `arrange` applies, so the fold override below and the
        // section filtering can't disagree about what "searching" means.
        let searching = !query.trim().is_empty();
        let saved = state.saved.read();
        let groups = saved.groups();
        let collapsed = state.collapsed_groups.read();
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
        saved
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
            .collect::<Vec<SectionView>>()
    });

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
            // Search and group creation. Both only appear once
            // there is a list to organise — on an empty screen they would be
            // two controls with nothing to act on.
            if has_saved {
                div { class: "flex w-full max-w-xl flex-col gap-2",
                    div { class: "flex items-center gap-2",
                        div { class: "flex min-w-0 flex-1 items-center gap-1 rounded border border-slate-300 dark:border-slate-700 bg-slate-100 dark:bg-slate-950 px-2 py-1 focus-within:border-sky-500 dark:focus-within:border-sky-600",
                            span { class: "shrink-0 text-slate-400 dark:text-slate-600",
                                Search { size: 14 }
                            }
                            TextInput {
                                id: "dv-connection-search",
                                class: "min-w-0 flex-1 bg-transparent text-sm text-slate-900 dark:text-slate-200 placeholder:text-slate-400 dark:placeholder:text-slate-600 focus:outline-none",
                                placeholder: "Search connections by name…",
                                text: "{query}",
                                oninput: move |evt: FormEvent| search.set(evt.value()),
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
                            TextInput {
                                class: "min-w-0 flex-1 rounded border border-slate-300 dark:border-slate-700 bg-slate-100 dark:bg-slate-950 px-2 py-1 text-sm text-slate-900 dark:text-slate-200",
                                placeholder: "Group name, e.g. Production",
                                text: "{new_group}",
                                onmounted: focus_on_mount,
                                oninput: move |evt: FormEvent| new_group.set(evt.value()),
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
            if has_saved && sections.read().is_empty() {
                p { class: "text-sm text-slate-500 dark:text-slate-400",
                    "No connection matches “{query}”."
                }
            }
            // Iterating the borrow: the memo keeps its value, so each row is
            // cloned into its props rather than moved.
            for section in sections.read().iter() {
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
                    if section.name.is_none() && has_groups {
                        div { class: "border-b border-slate-200 dark:border-slate-800 px-3 py-1.5 text-xs font-semibold uppercase tracking-wide text-slate-500",
                            "Ungrouped"
                        }
                    }
                    if !section.collapsed {
                        if section.rows.is_empty() {
                            p { class: "px-4 py-3 text-xs text-slate-500 dark:text-slate-400",
                                "No connections here yet — open a connection's shield button and pick this group."
                            }
                        }
                        ul { class: "divide-y divide-slate-200 dark:divide-slate-800",
                            for row in section.rows.iter() {
                                SavedConnectionRow {
                                    key: "{row.locator}",
                                    row: row.clone(),
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
                    class: "flex items-center gap-2 {BTN_PRIMARY} bg-sky-600 hover:bg-sky-500",
                    onclick: pick_file,
                    BackendIcon { dialect: Dialect::Sqlite, size: 16 }
                    "Add SQLite file…"
                }
                button {
                    class: "flex items-center gap-2 {BTN_PRIMARY} bg-cyan-700 hover:bg-cyan-600",
                    onclick: move |_| open_form.set(Some(ConnectForm::Postgres)),
                    BackendIcon { dialect: Dialect::Postgres, size: 16 }
                    "Add {BackendKind::Postgres.label()}…"
                }
                button {
                    class: "flex items-center gap-2 {BTN_PRIMARY} bg-red-800 hover:bg-red-700",
                    onclick: move |_| open_form.set(Some(ConnectForm::SqlServer)),
                    BackendIcon { dialect: Dialect::SqlServer, size: 16 }
                    "Add {BackendKind::SqlServer.label()}…"
                }
            }
            if let Some(kind) = open_form() {
                ConnectFormModal {
                    on_close: move |_| {
                        state.clear_pending_edit();
                        open_form.set(None);
                    },
                    error: error.clone(),
                    on_dismiss_error: move |_| state.connect_error.set(None),
                    match kind {
                        ConnectForm::Postgres => rsx! {
                            ServerForm {
                                key: "{ServerSpec::Postgres.form_key(None)}",
                                spec: ServerSpec::Postgres,
                                on_done: move |_| open_form.set(None),
                            }
                        },
                        ConnectForm::SqlServer => rsx! {
                            ServerForm {
                                key: "{ServerSpec::SqlServer.form_key(None)}",
                                spec: ServerSpec::SqlServer,
                                on_done: move |_| open_form.set(None),
                            }
                        },
                        ConnectForm::Edit(saved) => match ServerSpec::editing(&saved) {
                            Some(spec) => rsx! {
                                ServerForm {
                                    key: "{spec.form_key(Some(&saved))}",
                                    spec,
                                    edit: (*saved).clone(),
                                    on_done: move |_| open_form.set(None),
                                }
                            },
                            // The Edit action isn't offered on SQLite entries.
                            None => rsx! {},
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
                        on_dismiss: move |_| state.connect_error.set(None),
                    }
                }
            }
            if let (Some(message), None) = (notice, open_form()) {
                div { class: "w-full max-w-xl px-8",
                    Banner {
                        kind: BannerKind::Info,
                        message,
                        on_dismiss: move |_| state.connect_notice.set(None),
                    }
                }
            }
        }
    }
}

/// What the source pins over this module read: every file of it, found on
/// disk rather than listed, so a file added to the module is read without
/// anyone remembering to add it.
#[cfg(test)]
pub(in crate::ui) mod test_support {
    /// The shipped code of every file in this module, one after another, in
    /// path order — test modules left out, so a test naming a signature in a
    /// string is never found in its place.
    pub(in crate::ui) fn module_code() -> String {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/ui/connections");
        let mut files = crate::source_scan::rust_files(&dir);
        files.sort();
        assert!(
            files.len() >= 6,
            "found only {} files in the connections module",
            files.len()
        );
        // Only what the compiler builds may answer a pin: every file read is
        // declared `mod <stem>;` here, and every declaration has its file. A
        // stray copy left on disk — a correctly wired `PasswordPromptCard`
        // sorted ahead of the compiled one, say — would otherwise satisfy the
        // pins while the card that ships is inverted.
        let root =
            crate::source_scan::shipped(&std::fs::read_to_string(dir.join("mod.rs")).unwrap());
        let declared: Vec<String> = root
            .lines()
            .filter_map(|line| {
                let line = line.trim();
                let line = line.strip_prefix("pub(super) ").unwrap_or(line);
                line.strip_prefix("mod ")?
                    .strip_suffix(';')
                    .map(str::to_string)
            })
            .collect();
        let mut stems = Vec::new();
        for file in &files {
            assert_eq!(
                file.parent(),
                Some(dir.as_path()),
                "{}: a file below the module's directory is not read as one of its own",
                file.display()
            );
            let stem = file.file_stem().unwrap().to_string_lossy().into_owned();
            if stem != "mod" {
                assert!(
                    declared.contains(&stem),
                    "{}: on disk but not declared `mod {stem};` in connections/mod.rs, \
                     so the compiler never builds it",
                    file.display()
                );
                stems.push(stem);
            }
        }
        for module in &declared {
            assert!(
                stems.contains(module),
                "`mod {module};` has no file in src/ui/connections"
            );
        }
        files
            .iter()
            .map(|file| crate::source_scan::shipped(&std::fs::read_to_string(file).unwrap()))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// The shipped code of the item `signature` opens, in whichever file of
    /// this module it is, through the brace that closes it, as code alone:
    /// comments dropped, literals emptied, whitespace collapsed
    /// ([`crate::source_scan::code_only`]). Fails unless the signature occurs
    /// exactly once in the module.
    pub(in crate::ui) fn shipped_code_of(signature: &str) -> String {
        let shipped = module_code();
        let mut found = shipped.match_indices(signature);
        let (at, _) = found
            .next()
            .unwrap_or_else(|| panic!("no `{signature}` in the connections module"));
        assert!(
            found.next().is_none(),
            "`{signature}` occurs more than once; pin a longer signature"
        );
        let len = crate::source_scan::block_end(&shipped[at..])
            .unwrap_or_else(|| panic!("unbalanced braces after `{signature}`"));
        crate::source_scan::code_only(&shipped[at..at + len])
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::shipped_code_of;
    use super::*;

    #[test]
    fn every_remember_box_is_handed_the_value_its_hint_is_about() {
        // The hint functions are executed above; this pins what reaches them,
        // where an inversion compiles and no executed test sees it: the card
        // handed `StoredCopy::Absent` instead of what the prompt carries, a
        // form handed `false` instead of `editing`, or a form's hint built
        // from fields its submit does not use. Read as code alone
        // ([`crate::source_scan::code_only`]), so prose cannot satisfy it and
        // rustfmt cannot trip it.
        let body_of = shipped_code_of;
        for (signature, pins) in [
            (
                "fn PasswordPromptCard(prompt: crate::ui::state::PasswordPrompt) -> Element {",
                &["if let Some(hint) = remember_hint(prompt.kind, prompt.stored) {"][..],
            ),
            (
                "fn EntraSignInCard(prompt: crate::ui::state::EntraPrompt) -> Element {",
                &["if let Some(hint) = entra_remember_hint(prompt.stored) {"][..],
            ),
        ] {
            let body = body_of(signature);
            for pin in pins {
                assert!(body.contains(pin), "{signature}: `{pin}` is gone: {body}");
            }
        }
        let shared = [
            "let hint_old_locator = old_locator.clone();",
            "let remember_note = form_remember_hint( editing, &form_password( &password(), \
             embedded_url_password(use_url(), &pasted_url()), hint_old_locator.as_deref(), \
             hint_url.as_deref().unwrap_or_default(), ), hint_url.as_deref(), \
             state.saved.read().entries(), );",
        ];
        // One form serves both engines, so the hint and the submit build their
        // URL with the same call — the spec's builder over the one reader of
        // the fields — and each builds it exactly once.
        {
            let form = "fn ServerForm(";
            let builder_at_render = "let hint_url = spec.form_url(&form.fields()).ok();";
            let builder_at_submit = "let built = spec.form_url(&form.fields());";
            let body = body_of(form);
            assert_eq!(
                body.matches("form_url(").count(),
                2,
                "{form}: the URL is built somewhere besides the hint and the submit: {body}"
            );
            for pin in shared
                .iter()
                .copied()
                .chain([builder_at_render, builder_at_submit])
            {
                assert!(body.contains(pin), "{form}: `{pin}` is gone: {body}");
            }
            // The line renders inside the box's gate, after the box: shown
            // exactly when the box is, so it never speaks for a hidden one.
            let gate = "if asks_remember(auth_mode()) { label {";
            let at = body.find(gate).expect("the box's gate");
            let block = &body[at..];
            let block = &block[..block.find("div { class:").unwrap_or(block.len())];
            assert!(
                block.contains("if let Some(hint) = remember_note {"),
                "{form}: the hint is not inside the box's gate: {block}"
            );
            assert_eq!(
                body.matches("remember_note").count(),
                2,
                "{form}: the hint is bound once and rendered once: {body}"
            );
        }
        // The submit acts on the same decision, and on nothing else: the
        // password it hands on is `form_password`'s verdict, read once.
        let submit = body_of("fn submit_server_form(");
        for pin in [
            "let handed = form_password( &form.password.peek(), embedded_password, \
             old_locator.as_deref(), &url, );",
            "let entered_password = match handed { FormPassword::Entered(password) => password, \
             FormPassword::CarriedFrom(old) => crate::secrets::get_password_async(old) .await \
             .ok() .flatten() .unwrap_or_default(), FormPassword::Nothing => String::new(), };",
        ] {
            assert!(
                submit.contains(pin),
                "submit_server_form: `{pin}` is gone: {submit}"
            );
        }
        assert_eq!(
            submit.matches("entered_password =").count(),
            1,
            "submit_server_form decides its password somewhere besides `form_password`: {submit}"
        );
    }

    #[test]
    fn every_remember_box_starts_from_its_seed_and_records_what_is_ticked() {
        // The other two ends of each box's value: what it starts as, and what
        // a click writes back. `remember.set(!evt.checked())` records the
        // opposite of every click, and a seed of `false` (or `!offered`) asks
        // as if the user had already declined; every test that reads the box
        // further down the wire passes through both. Pinned as written, read
        // as code alone, for all three boxes: the connection form's, the
        // password prompt's and the Entra sign-in card's.
        let write_back = "checked: remember().checkbox(), onchange: move |evt| remember.set(Remember::from_checkbox(evt.checked())),";
        for (signature, seed) in [
            (
                "fn ServerForm(",
                &["remember: use_signal(|| Remember::Keyring),"][..],
            ),
            (
                "fn PasswordPromptCard(",
                &[
                    "let offered = prompt.remember;",
                    "let mut remember = use_signal(move || offered);",
                ][..],
            ),
            (
                "fn EntraSignInCard(",
                &[
                    "let offered = prompt.remember;",
                    "let mut remember = use_signal(move || offered);",
                ][..],
            ),
        ] {
            let code = shipped_code_of(signature);
            for pin in seed.iter().chain([&write_back]) {
                assert!(
                    code.contains(pin),
                    "{signature}: `{pin}` is gone, so the box starts from, or \
                     records, something other than the answer: {code}"
                );
            }
            assert_eq!(
                code.matches("remember.set(").count(),
                1,
                "{signature}: the box's value is written somewhere besides its \
                 own `onchange`: {code}"
            );
        }
        // The Entra card hands the box's value to its sign-in as read.
        assert!(
            shipped_code_of("fn EntraSignInCard(").contains("prompt.remember = *remember.peek();")
        );
    }

    #[tokio::test]
    async fn the_memoized_list_follows_every_signal_it_is_built_from() {
        // The list is a memo, so a signal it should read but only peeks at —
        // or one read in the render body instead of inside the memo — leaves
        // the list stale when that signal changes. One step per signal; each
        // asserts on what the change should put on screen.
        use std::path::PathBuf;

        use dioxus::core::{spawn_forever, Mutation, VirtualDom};

        use crate::db::{ConnectionRegistry, DbPool};
        use crate::ui::state::{saved_open_locator, ConnectRequest, Connecting};
        use crate::ui::test_dom::{listeners, provide_state, texts, type_into};

        let mut dom = VirtualDom::new(ConnectionsScreen);
        let state = provide_state(&dom);
        let path = PathBuf::from("/nonexistent/hubro-test/a.db");
        dom.in_runtime(|| {
            let mut saved = state.saved;
            let mut saved = saved.write();
            saved.create_group("G").unwrap();
            saved.add(SavedConnection::Sqlite {
                name: "alpha".into(),
                path: path.clone(),
                protection: WriteProtection::Open,
                color: None,
                group: Some("G".into()),
            });
        });
        let mounted = dom.rebuild_to_vec().edits;
        assert!(texts(&mounted).contains(&"alpha"), "{mounted:#?}");
        let search = listeners(&mounted, "input");
        assert_eq!(search.len(), 1, "the search box should be the only input");
        let locator = dom.in_runtime(|| state.saved.peek().entries()[0].locator());
        let canonical = dom.in_runtime(|| saved_open_locator(&state.saved.peek().entries()[0]));

        // `saved`: the row's marking shows as a badge.
        dom.in_runtime(|| {
            let mut saved = state.saved;
            saved
                .write()
                .set_marking(&locator, WriteProtection::Confirm, None);
        });
        let edits = dom.render_immediate_to_vec().edits;
        assert!(
            texts(&edits).contains(&"confirm writes"),
            "saved: {edits:#?}"
        );

        // `open_locators`: the "open" badge — static text, so it shows as
        // edits; nothing outside the memo reads the list. Each step writes
        // only its own signal: a second signal the memo reads, written in the
        // same step, would rebuild the list whether or not this one is read.
        dom.in_runtime(|| {
            let pool =
                DbPool::Sqlite(sqlx::sqlite::SqlitePool::connect_lazy("sqlite::memory:").unwrap());
            let id = ConnectionRegistry::default().insert("a.db", pool, WriteProtection::Open);
            let mut open = state.open_locators;
            open.write().push((id, canonical.clone()));
        });
        let edits = dom.render_immediate_to_vec().edits;
        assert!(
            !edits.is_empty(),
            "open_locators: nothing on screen changed"
        );

        // `connecting`: the row's locator line becomes the connect step.
        dom.in_runtime(|| {
            let mut connecting = state.connecting;
            connecting.write().push(Connecting {
                locator: canonical.clone(),
                step: ConnectStep::Opening,
                visible: true,
            });
        });
        let edits = dom.render_immediate_to_vec().edits;
        assert!(
            texts(&edits).contains(&ConnectStep::Opening.label()),
            "connecting: {edits:#?}"
        );

        // `connect_requests`: a connect started from the list can be called
        // off. The Cancel button is all static template, so it shows as
        // edits rather than text; nothing outside the memo reads the map, so
        // any edit at all is the memo's.
        dom.in_runtime(|| {
            let task = spawn_forever(std::future::pending());
            let mut requests = state.connect_requests;
            requests
                .write()
                .insert(canonical.clone(), ConnectRequest::for_test(task));
        });
        let edits = dom.render_immediate_to_vec().edits;
        assert!(
            !edits.is_empty(),
            "connect_requests: nothing on screen changed"
        );

        // `collapsed_groups`: folding the group takes its row off screen.
        dom.in_runtime(|| {
            let mut collapsed = state.collapsed_groups;
            collapsed.write().push("G".into());
        });
        let edits = dom.render_immediate_to_vec().edits;
        assert!(
            edits
                .iter()
                .any(|edit| matches!(edit, Mutation::Remove { .. })),
            "collapsed_groups: {edits:#?}"
        );

        // `search`: the render body reads it too (the field's value), so an
        // edit proves nothing here. Only the memo can empty the list, and
        // only an empty list says nothing matched.
        type_into(&dom, search[0], "zzz");
        let edits = dom.render_immediate_to_vec().edits;
        assert!(
            texts(&edits).contains(&"No connection matches “zzz”."),
            "search: {edits:#?}"
        );
    }
}
