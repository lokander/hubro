use std::path::PathBuf;
use std::time::Duration;

use dioxus::desktop::tao::dpi::PhysicalPosition;
use dioxus::desktop::tao::event::{Event, WindowEvent};
use dioxus::desktop::{use_window, use_wry_event_handler, DesktopService, WindowCloseBehaviour};
use dioxus::prelude::*;

use crate::config::{default_settings_path, save_window_geometry, Theme, WindowGeometry};
use crate::db::ConnectionId;

use super::editor::SqlEditor;
use super::grid::DataGrid;
use super::notice::{Banner, BannerKind, EmptyState};
use super::sidebar::SchemaSidebar;
use super::state::{ActiveView, AppState};

/// The window-level keyboard listener (FRE-15). Installed once as a plain
/// `keydown` handler on `window` — a webview-robust way to capture app-global
/// shortcuts regardless of which element holds focus (a focusable Dioxus
/// wrapper would only see keys while it, and not the sidebar/grid/buttons,
/// had focus). It self-guards against text-entry contexts (inputs, the cell
/// editor, CodeMirror) so typing never triggers a shortcut, handles the
/// focus-only shortcuts entirely in JS (focus the filter, focus + arrow
/// through the sidebar table list), and forwards the state-changing ones to
/// Rust via `dioxus.send`. The `__dvKeys` guard makes a re-install (e.g. dev
/// hot-reload) a no-op.
const GLOBAL_KEYS_JS: &str = r#"
(() => {
  if (window.__dvKeys) return;
  window.__dvKeys = true;
  const typing = (el) => {
    if (!el) return false;
    const tag = el.tagName;
    if (tag === 'INPUT' || tag === 'TEXTAREA' || tag === 'SELECT') return true;
    if (el.isContentEditable) return true;
    return !!(el.closest && el.closest('.cm-editor'));
  };
  const tableButtons = () => Array.from(document.querySelectorAll('.dv-table-btn'));
  window.addEventListener('keydown', (e) => {
    const el = document.activeElement;
    // Sidebar table-list navigation: a focused table button (not a text
    // field) arrows between siblings; Enter/Space open it natively.
    if (el && el.classList && el.classList.contains('dv-table-btn')) {
      const btns = tableButtons();
      const i = btns.indexOf(el);
      if (e.key === 'ArrowDown') { e.preventDefault(); if (i >= 0 && i + 1 < btns.length) btns[i + 1].focus(); return; }
      if (e.key === 'ArrowUp')   { e.preventDefault(); if (i > 0) btns[i - 1].focus(); return; }
      if (e.key === 'Home')      { e.preventDefault(); if (btns.length) btns[0].focus(); return; }
      if (e.key === 'End')       { e.preventDefault(); if (btns.length) btns[btns.length - 1].focus(); return; }
    }
    if (typing(el)) return;
    if (e.key === '?') { e.preventDefault(); dioxus.send('cheatsheet'); return; }
    if (e.key === 'Escape') { dioxus.send('escape'); return; }
    if (e.key === '/') {
      const f = document.getElementById('dv-filter');
      if (f) { e.preventDefault(); f.focus(); }
      return;
    }
    if (e.ctrlKey && (e.key === 'e' || e.key === 'E')) { e.preventDefault(); dioxus.send('pane'); return; }
    if (e.ctrlKey && (e.key === 'b' || e.key === 'B')) {
      e.preventDefault();
      const btns = tableButtons();
      const cur = btns.find((b) => b.getAttribute('data-selected') === 'true') || btns[0];
      if (cur) cur.focus();
      return;
    }
  });
})();
"#;

/// Top-level layout: tab bar over the active view.
#[component]
pub fn Shell() -> Element {
    let state = use_context::<AppState>();
    let active = *state.active.read();
    let dark = state.dark;
    let show_cheatsheet = state.show_cheatsheet;

    // Install the window-level shortcut listener once and pump the keys it
    // forwards. The eval channel stays open for the app's lifetime; reading
    // no signals here keeps the effect from re-running (and the JS guard
    // would ignore a re-install anyway).
    use_effect(move || {
        spawn(async move {
            let mut channel = document::eval(GLOBAL_KEYS_JS);
            while let Ok(msg) = channel.recv::<String>().await {
                match msg.as_str() {
                    "cheatsheet" => state.toggle_cheatsheet(),
                    "escape" => state.close_cheatsheet(),
                    "pane" => state.toggle_active_pane(),
                    _ => {}
                }
            }
        });
    });

    // Restore the previous session exactly once, from this component's scope
    // (not a root `spawn_forever` in AppState::new): restore drives the normal
    // connect flow, which writes the core connection signals — running it here
    // keeps those writes in a live scope, matching the manual connect path.
    use_hook(|| {
        spawn(async move {
            state.restore_session().await;
        });
    });

    // Session persistence (FRE-30): re-snapshot whenever the open tabs, their
    // selected table/pane, or the active view change, and write session.toml
    // only when the snapshot actually differs. `current_session` reads
    // `open_locators`, `tab_ui`, and `active`, so this effect re-runs on any
    // of them — including per-keystroke `tab_ui` changes from the SQL editor,
    // which diff to an identical snapshot and skip the write.
    let mut last_session = use_signal(|| state.current_session());
    use_effect(move || {
        let current = state.current_session();
        if current != *last_session.peek() {
            last_session.set(current);
            state.persist_session();
        }
    });

    rsx! {
        // The `.dark` class gates every `dark:` utility below it (see the
        // @custom-variant in tailwind.css); toggling it swaps the theme.
        div {
            class: if dark() {
                "dark flex h-screen flex-col bg-white dark:bg-slate-900 text-slate-900 dark:text-slate-100"
            } else {
                "flex h-screen flex-col bg-white dark:bg-slate-900 text-slate-900 dark:text-slate-100"
            },
            WindowPersistence {}
            TabBar {}
            main { class: "min-h-0 flex-1",
                match active {
                    ActiveView::Connections => rsx! { ConnectionsScreen {} },
                    // Keyed so per-tab hook state never leaks across tabs.
                    ActiveView::Connection(id) => rsx! { ConnectionView { key: "{id:?}", id } },
                }
            }
            // Rendered inside the `.dark`-scoped root so the overlay themes
            // with the app.
            if show_cheatsheet() {
                Cheatsheet {}
            }
            if state.confirm_quit.read().to_owned() {
                QuitConfirmDialog {}
            }
        }
    }
}

/// Confirmation shown when the user tries to close the window with unsaved
/// staged edits (FRE-37). The close was vetoed (the window switched to
/// hide-on-close); here the user either discards and quits for real or cancels
/// back into the app to save.
#[component]
fn QuitConfirmDialog() -> Element {
    let mut state = use_context::<AppState>();
    let window = use_window();
    rsx! {
        div {
            class: "fixed inset-0 z-50 flex items-center justify-center bg-black/50 p-4",
            div { class: "w-full max-w-md rounded-lg border border-slate-300 dark:border-slate-700 bg-white dark:bg-slate-900 p-5 shadow-xl",
                h2 { class: "text-base font-semibold text-slate-900 dark:text-slate-100",
                    "Discard unsaved changes?"
                }
                p { class: "mt-2 text-sm text-slate-600 dark:text-slate-400",
                    "You have edits that haven't been saved to the database. Quitting now discards them."
                }
                div { class: "mt-5 flex justify-end gap-2",
                    button {
                        class: "rounded px-3 py-2 text-sm text-slate-600 dark:text-slate-300 hover:bg-slate-100 dark:hover:bg-slate-800",
                        onclick: move |_| state.confirm_quit.set(false),
                        "Cancel"
                    }
                    button {
                        class: "rounded bg-rose-600 px-4 py-2 text-sm font-medium text-white hover:bg-rose-500",
                        onclick: move |_| {
                            // Stop the re-show loop, re-arm a real close, then
                            // request it. `close()` routes through the same
                            // handler, which now sees WindowCloses and lets the
                            // window go.
                            state.confirm_quit.set(false);
                            window.set_close_behavior(WindowCloseBehaviour::WindowCloses);
                            window.close();
                        },
                        "Discard changes and quit"
                    }
                }
            }
        }
    }
}

/// Reads the window's current geometry in logical pixels. Logical (not
/// physical) so a saved geometry restores sanely across monitors of different
/// DPI. An unavailable outer position (some platforms) just drops the
/// coordinates.
fn read_geometry(window: &DesktopService) -> WindowGeometry {
    let scale = window.scale_factor();
    let size = window.inner_size().to_logical::<f64>(scale);
    let position = window
        .outer_position()
        .ok()
        .map(|p: PhysicalPosition<i32>| p.to_logical::<f64>(scale));
    WindowGeometry {
        width: size.width,
        height: size.height,
        x: position.map(|p| p.x),
        y: position.map(|p| p.y),
        maximized: window.is_maximized(),
    }
}

/// Updates the in-memory geometry from a resize/move. While maximized the
/// size/position are the maximized bounds, so we keep the last restored ones
/// (un-maximizing returns to them) and only remember the maximized flag.
fn update_geometry(latest: &mut Signal<WindowGeometry>, window: &DesktopService) {
    if window.is_maximized() {
        latest.with_mut(|g| g.maximized = true);
    } else {
        latest.set(read_geometry(window));
    }
}

/// Captures window size/position and persists it to settings.toml (FRE-30) so
/// the next launch restores it. Renders nothing.
///
/// Mechanism: a `use_wry_event_handler` updates an in-memory geometry on every
/// `Resized`/`Moved` (cheap, no file I/O) and writes the final value on
/// `CloseRequested`; a 1 s poll writes it when it settles, so dragging the
/// window produces one write, not one per pixel. Writes are best-effort — a
/// failure only means the geometry won't survive this restart, never a crash.
#[component]
fn WindowPersistence() -> Element {
    let state = use_context::<AppState>();
    let window = use_window();
    let mut latest = use_signal(|| read_geometry(&window));

    {
        let window = window.clone();
        use_wry_event_handler(move |event, _| match event {
            Event::WindowEvent {
                event: WindowEvent::Resized(_) | WindowEvent::Moved(_),
                ..
            } => update_geometry(&mut latest, &window),
            Event::WindowEvent {
                event: WindowEvent::CloseRequested,
                ..
            } => {
                update_geometry(&mut latest, &window);
                if let Some(path) = default_settings_path() {
                    let _ = save_window_geometry(&path, latest.peek().sanitized());
                }
                // This handler runs (via `app.tick`) *before* the built-in
                // close handler reads the close behaviour, so setting it here
                // decides what that close does. With unsaved edits, switch to
                // hide-on-close to veto the destroy and raise the confirmation
                // (the `_` arm below re-shows the just-hidden window).
                if state.any_dirty() {
                    window.set_close_behavior(WindowCloseBehaviour::WindowHides);
                    state.confirm_quit.clone().set(true);
                } else {
                    window.set_close_behavior(WindowCloseBehaviour::WindowCloses);
                }
            }
            // The vetoed close hides the window (the only non-destructive
            // option the desktop shell offers). Dioxus pauses its render loop
            // while the webview is hidden, so a `use_effect` can't bring it
            // back — but this handler runs for every raw event, so re-show it
            // here (idempotent) while the confirmation is pending. The hide
            // itself emits events, so the window bounces straight back.
            _ => {
                if *state.confirm_quit.peek() {
                    window.set_visible(true);
                }
            }
        });
    }

    // Debounced writer: at most one write per second, and only on a real
    // change. `peek` avoids subscribing, so this future runs exactly once.
    use_future(move || async move {
        let mut persisted = *latest.peek();
        loop {
            tokio::time::sleep(Duration::from_secs(1)).await;
            let current = *latest.peek();
            if current != persisted {
                persisted = current;
                if let Some(path) = default_settings_path() {
                    let _ = save_window_geometry(&path, current.sanitized());
                }
            }
        }
    });

    rsx! {}
}

/// One shortcut row: a description and its key(s).
#[component]
fn ShortcutRow(keys: &'static str, desc: &'static str) -> Element {
    rsx! {
        div { class: "flex items-center justify-between gap-6 py-1",
            span { class: "text-sm text-slate-700 dark:text-slate-300", "{desc}" }
            kbd { class: "shrink-0 rounded border border-slate-300 dark:border-slate-600 bg-slate-100 dark:bg-slate-800 px-1.5 py-0.5 font-mono text-xs text-slate-700 dark:text-slate-200",
                "{keys}"
            }
        }
    }
}

/// A group heading in the cheatsheet.
#[component]
fn ShortcutGroup(title: &'static str, children: Element) -> Element {
    rsx! {
        div {
            h3 { class: "mb-1 text-xs font-semibold uppercase tracking-wide text-slate-500 dark:text-slate-400",
                "{title}"
            }
            {children}
        }
    }
}

/// The `?` shortcut cheatsheet (FRE-15): a modal overlay grouping every
/// keybinding, dismissed by Escape (handled by the global listener), a click
/// on the backdrop, or `?` again. Kept in sync by hand with the actual
/// bindings in [`GLOBAL_KEYS_JS`] and the grid's key handler.
#[component]
fn Cheatsheet() -> Element {
    let state = use_context::<AppState>();
    rsx! {
        // Backdrop: a click anywhere outside the panel closes.
        div {
            class: "fixed inset-0 z-50 flex items-center justify-center bg-black/40 p-4",
            onclick: move |_| state.close_cheatsheet(),
            div {
                // Stop clicks inside the panel from reaching the backdrop.
                onclick: move |evt| evt.stop_propagation(),
                class: "max-h-[85vh] w-full max-w-lg overflow-y-auto rounded-lg border border-slate-300 dark:border-slate-700 bg-white dark:bg-slate-900 p-5 shadow-xl",
                div { class: "mb-4 flex items-center justify-between",
                    h2 { class: "text-lg font-semibold text-slate-900 dark:text-slate-100",
                        "Keyboard shortcuts"
                    }
                    button {
                        class: "rounded px-2 py-1 text-sm text-slate-500 dark:text-slate-400 hover:bg-slate-200 dark:hover:bg-slate-800 hover:text-slate-900 dark:hover:text-slate-100",
                        aria_label: "Close",
                        onclick: move |_| state.close_cheatsheet(),
                        "✕"
                    }
                }
                div { class: "grid grid-cols-1 gap-5 sm:grid-cols-2",
                    ShortcutGroup { title: "Data grid",
                        ShortcutRow { keys: "↑ ↓ ← →", desc: "Move the focused cell" }
                        ShortcutRow { keys: "Home / End", desc: "First / last cell in the row" }
                        ShortcutRow { keys: "Ctrl+Home / End", desc: "First / last cell on the page" }
                        ShortcutRow { keys: "PageUp / PageDown", desc: "Previous / next page" }
                        ShortcutRow { keys: "Enter", desc: "Edit the cell, or show its full value" }
                        ShortcutRow { keys: "Esc", desc: "Close the value popup" }
                    }
                    ShortcutGroup { title: "Navigation",
                        ShortcutRow { keys: "/", desc: "Focus the filter box" }
                        ShortcutRow { keys: "Ctrl+B", desc: "Focus the table list" }
                        ShortcutRow { keys: "↑ ↓ / Enter", desc: "Move / open a table (in the list)" }
                        ShortcutRow { keys: "Ctrl+E", desc: "Switch Data / SQL pane" }
                        ShortcutRow { keys: "?", desc: "Toggle this help" }
                        ShortcutRow { keys: "Esc", desc: "Close this help" }
                    }
                    ShortcutGroup { title: "Cell editor",
                        ShortcutRow { keys: "Enter", desc: "Commit the edit" }
                        ShortcutRow { keys: "Esc", desc: "Cancel the edit" }
                        ShortcutRow { keys: "Tab / Shift+Tab", desc: "Next / previous field" }
                        ShortcutRow { keys: "Double-click", desc: "Edit a cell with the mouse" }
                    }
                    ShortcutGroup { title: "SQL editor",
                        ShortcutRow { keys: "Ctrl+Enter", desc: "Run the buffer or selection" }
                    }
                }
            }
        }
    }
}

/// One tab per open connection, plus a fixed tab for the connections screen.
#[component]
fn TabBar() -> Element {
    let mut state = use_context::<AppState>();
    let active = *state.active.read();
    // Owned copies so the loop can hand ids/names to event handlers.
    let tabs: Vec<(ConnectionId, String)> = state
        .registry
        .read()
        .iter()
        .map(|c| (c.id, c.name.clone()))
        .collect();
    rsx! {
        header { class: "flex items-center gap-1 border-b border-slate-300 dark:border-slate-700 bg-slate-100 dark:bg-slate-950 px-2 pt-1",
            button {
                class: if active == ActiveView::Connections {
                    "rounded-t px-3 py-1.5 text-sm bg-white dark:bg-slate-900 text-slate-900 dark:text-slate-100"
                } else {
                    "rounded-t px-3 py-1.5 text-sm text-slate-500 dark:text-slate-400 hover:text-slate-900 dark:hover:text-slate-100"
                },
                onclick: move |_| state.active.set(ActiveView::Connections),
                "Connections"
            }
            for (id, name) in tabs {
                div {
                    class: if active == ActiveView::Connection(id) {
                        "flex items-center gap-1 rounded-t bg-white dark:bg-slate-900 px-3 py-1.5 text-sm text-slate-900 dark:text-slate-100"
                    } else {
                        "flex items-center gap-1 rounded-t px-3 py-1.5 text-sm text-slate-500 dark:text-slate-400 hover:text-slate-900 dark:hover:text-slate-100"
                    },
                    button {
                        onclick: move |_| state.active.set(ActiveView::Connection(id)),
                        "{name}"
                    }
                    button {
                        class: "rounded px-1 text-slate-500 hover:bg-slate-300 dark:hover:bg-slate-700 hover:text-slate-900 dark:hover:text-slate-200",
                        aria_label: "Close connection",
                        onclick: move |_| state.close_connection(id),
                        "×"
                    }
                }
            }
            ThemeToggle {}
        }
    }
}

/// Right-aligned control in the tab bar that cycles System → Light → Dark.
#[component]
fn ThemeToggle() -> Element {
    let state = use_context::<AppState>();
    let theme = *state.theme.read();
    let icon = match theme {
        Theme::System => "◐",
        Theme::Light => "☀",
        Theme::Dark => "☾",
    };
    rsx! {
        button {
            class: "ml-auto rounded px-2 py-1 text-xs text-slate-500 dark:text-slate-400 hover:bg-slate-200 dark:hover:bg-slate-800 hover:text-slate-900 dark:hover:text-slate-100",
            title: "Theme (click to cycle System / Light / Dark)",
            aria_label: "Toggle theme",
            onclick: move |_| state.set_theme(theme.next()),
            "{icon} {theme.label()}"
        }
    }
}

/// Layout for one open connection: schema sidebar left, data grid right.
#[component]
fn ConnectionView(id: ConnectionId) -> Element {
    let state = use_context::<AppState>();
    let name = state.registry.read().get(id).map(|c| c.name.clone());
    let Some(name) = name else {
        // Tab was closed under us; the view switches away on the next render.
        return rsx! {
            div { class: "p-8 text-slate-500 dark:text-slate-400", "This connection is closed." }
        };
    };
    let (selected, pane) = {
        let tab_ui = state.tab_ui.read();
        let ui = tab_ui.get(&id);
        (
            ui.and_then(|ui| ui.selected_table.clone()),
            ui.map(|ui| ui.pane).unwrap_or_default(),
        )
    };
    rsx! {
        div { class: "flex h-full",
            aside { class: "flex w-72 shrink-0 flex-col border-r border-slate-300 dark:border-slate-700 bg-slate-50 dark:bg-slate-950/50",
                h2 { class: "border-b border-slate-200 dark:border-slate-800 px-4 py-3 font-mono text-sm text-slate-900 dark:text-slate-300",
                    "{name}"
                }
                SchemaSidebar { id }
            }
            section { class: "flex min-w-0 flex-1 flex-col",
                div { class: "flex gap-1 border-b border-slate-200 dark:border-slate-800 px-3 py-1.5",
                    PaneButton { id, pane, target: super::state::Pane::Browser, label: "Data" }
                    PaneButton { id, pane, target: super::state::Pane::Sql, label: "SQL" }
                }
                match pane {
                    super::state::Pane::Sql => rsx! {
                        SqlEditor { key: "sql-{id:?}", id }
                    },
                    super::state::Pane::Browser => match selected {
                        // Keyed by table so grid state (page, sort, filter)
                        // resets when another table is selected.
                        Some(table) => rsx! {
                            DataGrid { key: "{table.key()}", id, table: table.clone() }
                        },
                        None => rsx! {
                            div { class: "flex flex-1 items-center justify-center",
                                p { class: "text-slate-500", "Select a table to view its data." }
                            }
                        },
                    },
                }
            }
        }
    }
}

/// One segment of the Data / SQL pane switch.
#[component]
fn PaneButton(
    id: ConnectionId,
    pane: super::state::Pane,
    target: super::state::Pane,
    label: String,
) -> Element {
    let state = use_context::<AppState>();
    rsx! {
        button {
            class: if pane == target {
                "rounded px-3 py-0.5 text-xs bg-slate-300 dark:bg-slate-700 text-slate-900 dark:text-slate-100"
            } else {
                "rounded px-3 py-0.5 text-xs text-slate-500 dark:text-slate-400 hover:bg-slate-200 dark:hover:bg-slate-800 hover:text-slate-900 dark:hover:text-slate-100"
            },
            onclick: move |_| state.set_pane(id, target),
            "{label}"
        }
    }
}

/// One row of the saved list, precomputed for rendering.
#[derive(Clone, PartialEq)]
struct SavedRow {
    name: String,
    locator: String,
    is_postgres: bool,
    is_open: bool,
    tunnel: Option<crate::tunnel::TunnelConfig>,
}

/// Launch screen: the persisted saved-connections list plus add flows for
/// SQLite (native file picker) and Postgres (form or URL).
#[component]
fn ConnectionsScreen() -> Element {
    let state = use_context::<AppState>();
    let mut show_pg_form = use_signal(|| false);
    let error = state.connect_error.read().clone();
    let prompt = state.password_prompt.read().clone();
    let host_key_prompt = state.host_key_prompt.read().clone();
    let saved: Vec<SavedRow> = {
        let open = state.open_locators.read();
        state
            .saved
            .read()
            .entries()
            .iter()
            .map(|s| {
                let canonical_locator = match s {
                    crate::config::SavedConnection::Sqlite { path, .. } => {
                        super::state::canonical(path).display().to_string()
                    }
                    crate::config::SavedConnection::Postgres { url, .. } => url.clone(),
                };
                let tunnel = match s {
                    crate::config::SavedConnection::Postgres { tunnel, .. } => tunnel.clone(),
                    _ => None,
                };
                SavedRow {
                    name: s.name().to_string(),
                    locator: s.locator(),
                    is_postgres: matches!(s, crate::config::SavedConnection::Postgres { .. }),
                    is_open: open.iter().any(|(_, l)| *l == canonical_locator),
                    tunnel,
                }
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
                h1 { class: "text-2xl font-semibold text-slate-900 dark:text-slate-200", "dataview" }
                if !saved.is_empty() {
                    p { class: "mt-1 text-sm text-slate-500 dark:text-slate-400",
                        "Pick a saved connection, or add another database."
                    }
                }
            }
            if saved.is_empty() {
                // Designed empty state; the Add buttons below are its action.
                EmptyState {
                    icon: "\u{1F50C}", // 🔌 plug
                    title: "No connections yet",
                    hint: "Add a SQLite file or Postgres server to get started.",
                }
            }
            if !saved.is_empty() {
                ul { class: "w-full max-w-xl divide-y divide-slate-200 dark:divide-slate-800 rounded border border-slate-300 dark:border-slate-700 bg-slate-50 dark:bg-slate-950/60",
                    for row in saved {
                        li { class: "flex items-center gap-3 px-4 py-3",
                            button {
                                class: "min-w-0 flex-1 text-left",
                                onclick: {
                                    let row = row.clone();
                                    move |_| {
                                        let row = row.clone();
                                        spawn(async move {
                                            if row.is_postgres {
                                                state
                                                    .connect_postgres(row.locator, row.name, row.tunnel)
                                                    .await;
                                            } else {
                                                state.connect(PathBuf::from(row.locator)).await;
                                            }
                                        });
                                    }
                                },
                                div { class: "flex items-center gap-2",
                                    span { class: "truncate text-sm font-medium text-slate-900 dark:text-slate-200",
                                        "{row.name}"
                                    }
                                    span {
                                        class: if row.is_postgres {
                                            "rounded bg-cyan-100 dark:bg-cyan-900/50 px-1.5 py-0.5 text-xs text-cyan-700 dark:text-cyan-300"
                                        } else {
                                            "rounded bg-slate-200 dark:bg-slate-800 px-1.5 py-0.5 text-xs text-slate-500 dark:text-slate-400"
                                        },
                                        if row.is_postgres { "postgres" } else { "sqlite" }
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
                                }
                                div { class: "truncate font-mono text-xs text-slate-500",
                                    "{row.locator}"
                                }
                            }
                            button {
                                class: "rounded px-2 py-1 text-xs text-slate-500 hover:bg-slate-200 dark:hover:bg-slate-800 hover:text-slate-900 dark:hover:text-slate-200",
                                aria_label: "Remove saved connection",
                                onclick: {
                                    let locator = row.locator.clone();
                                    move |_| state.remove_saved(&locator)
                                },
                                "Remove"
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
            div { class: "flex gap-3",
                button {
                    class: "rounded bg-sky-600 px-4 py-2 text-sm font-medium text-white hover:bg-sky-500",
                    onclick: pick_file,
                    "Add SQLite file…"
                }
                button {
                    class: "rounded bg-cyan-700 px-4 py-2 text-sm font-medium text-white hover:bg-cyan-600",
                    onclick: move |_| {
                        let showing = *show_pg_form.read();
                        show_pg_form.set(!showing);
                    },
                    "Add Postgres…"
                }
            }
            if show_pg_form() {
                PostgresForm { on_done: move |_| show_pg_form.set(false) }
            }
            if let Some(err) = error {
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

/// Inline secret prompt for a saved Postgres connection: the database
/// password, or the SSH key passphrase when the tunnel's key is encrypted.
/// "Remember" stores the secret in the OS keyring; without it (or when no
/// keyring is available) it lives in session memory only.
#[component]
fn PasswordPromptCard(prompt: super::state::PasswordPrompt) -> Element {
    use super::state::PromptKind;
    let state = use_context::<AppState>();
    let mut password = use_signal(String::new);
    let mut remember = use_signal(|| true);
    let prompt_for_submit = prompt.clone();
    let submit = move || {
        let prompt = prompt_for_submit.clone();
        let entered = password.peek().clone();
        let remember_choice = *remember.peek();
        // spawn_forever: the connect flow clears `password_prompt`, which
        // unmounts this card — a scope-tied `spawn` would be cancelled at
        // its next await, silently abandoning the connect.
        dioxus::core::spawn_forever(async move {
            match prompt.kind {
                PromptKind::DbPassword => {
                    state
                        .connect_postgres_with_password(
                            prompt.url,
                            prompt.name,
                            entered,
                            remember_choice,
                            prompt.tunnel,
                        )
                        .await;
                }
                PromptKind::SshPassphrase => {
                    // An SSH prompt always carries its tunnel config.
                    let Some(tunnel) = prompt.tunnel else { return };
                    state
                        .connect_postgres_with_ssh_passphrase(
                            prompt.url,
                            prompt.name,
                            tunnel,
                            entered,
                            remember_choice,
                        )
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
                    onclick: move |_| state.password_prompt.clone().set(None),
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
/// records the key in dataview's known_hosts store and retries the connect.
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
                    onclick: move |_| state.host_key_prompt.clone().set(None),
                    "Cancel"
                }
            }
        }
    }
}

/// Add-Postgres panel: individual fields or a pasted URL, plus an optional
/// SSH tunnel.
#[component]
fn PostgresForm(on_done: EventHandler<()>) -> Element {
    use crate::tunnel::{TunnelAuth, TunnelConfig};
    let state = use_context::<AppState>();
    let mut use_url = use_signal(|| false);
    let mut name = use_signal(String::new);
    let mut host = use_signal(String::new);
    let mut port = use_signal(String::new);
    let mut database = use_signal(String::new);
    let mut user = use_signal(String::new);
    let mut password = use_signal(String::new);
    let mut remember = use_signal(|| true);
    let mut sslmode = use_signal(|| "prefer".to_string());
    let mut pasted_url = use_signal(String::new);
    let mut use_tunnel = use_signal(|| false);
    let mut ssh_host = use_signal(String::new);
    let mut ssh_port = use_signal(String::new);
    let mut ssh_user = use_signal(String::new);
    // false = ssh-agent (the default), true = key file.
    let mut ssh_use_key = use_signal(|| false);
    let mut ssh_key_path = use_signal(String::new);
    let mut ssh_passphrase = use_signal(String::new);
    let mut form_error = use_signal(|| Option::<String>::None);

    let mut submit = move || {
        // Tunnel settings are validated first so a bad SSH field fails
        // before any connect attempt.
        let tunnel: Option<TunnelConfig> = if *use_tunnel.peek() {
            let host = ssh_host.peek().trim().to_string();
            if host.is_empty() {
                form_error.set(Some("SSH host must not be empty".to_string()));
                return;
            }
            let port_text = ssh_port.peek().trim().to_string();
            let port = if port_text.is_empty() {
                22
            } else {
                match port_text.parse() {
                    Ok(port) => port,
                    Err(_) => {
                        form_error.set(Some(format!("invalid SSH port: {port_text}")));
                        return;
                    }
                }
            };
            let user = ssh_user.peek().trim().to_string();
            if user.is_empty() {
                form_error.set(Some("SSH user must not be empty".to_string()));
                return;
            }
            let auth = if *ssh_use_key.peek() {
                let path = ssh_key_path.peek().trim().to_string();
                if path.is_empty() {
                    form_error.set(Some("SSH key file path must not be empty".to_string()));
                    return;
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
            Some(TunnelConfig {
                host,
                port,
                user,
                auth,
            })
        } else {
            None
        };
        let entered_passphrase = if matches!(
            tunnel,
            Some(TunnelConfig {
                auth: TunnelAuth::KeyFile { .. },
                ..
            })
        ) {
            Some(ssh_passphrase.peek().clone()).filter(|p| !p.is_empty())
        } else {
            None
        };
        // A password pasted inside the URL is used for this connect (and
        // remembered for the session on success) but never persisted.
        let embedded_password = if *use_url.peek() {
            url::Url::parse(pasted_url.peek().trim())
                .ok()
                .and_then(|u| {
                    // Url::password() returns the still-encoded form.
                    u.password().map(|p| {
                        percent_encoding::percent_decode_str(p)
                            .decode_utf8_lossy()
                            .into_owned()
                    })
                })
        } else {
            None
        };
        let built = if *use_url.peek() {
            crate::db::sanitized_url(&pasted_url.peek())
        } else {
            crate::db::build_url(
                &host.peek(),
                &port.peek(),
                &database.peek(),
                &user.peek(),
                &sslmode.peek(),
            )
        };
        let url = match built {
            Ok(url) => url,
            Err(err) => {
                form_error.set(Some(err.to_string()));
                return;
            }
        };
        let display_name = {
            let entered = name.peek().trim().to_string();
            if entered.is_empty() {
                default_pg_name(&url)
            } else {
                entered
            }
        };
        let mut entered_password = password.peek().clone();
        if entered_password.is_empty() {
            if let Some(embedded) = embedded_password {
                entered_password = embedded;
            }
        }
        let remember_choice = *remember.peek();
        form_error.set(None);
        spawn(async move {
            // An entered passphrase seeds session memory so the tunnel open
            // finds it, exactly as if it came from the prompt.
            if let Some(passphrase) = &entered_passphrase {
                state.stash_ssh_passphrase(&url, passphrase.clone());
            }
            if entered_password.is_empty() {
                state
                    .connect_postgres(url.clone(), display_name.clone(), tunnel.clone())
                    .await;
            } else {
                state
                    .connect_postgres_with_password(
                        url.clone(),
                        display_name.clone(),
                        entered_password,
                        remember_choice,
                        tunnel.clone(),
                    )
                    .await;
            }
            // Only save and close the form when the connection worked.
            if state.open_locators.peek().iter().any(|(_, l)| *l == url) {
                if remember_choice && entered_passphrase.is_some() {
                    state.persist_ssh_passphrase(&url).await;
                }
                state.add_saved_postgres(display_name, url, tunnel);
                on_done.call(());
            }
        });
    };

    let field_class = "w-full rounded border border-slate-300 dark:border-slate-700 bg-slate-100 dark:bg-slate-950 px-3 py-2 font-mono text-sm text-slate-900 dark:text-slate-200 placeholder:text-slate-400 dark:placeholder:text-slate-600";
    rsx! {
        div { class: "w-full max-w-xl rounded border border-slate-300 dark:border-slate-700 bg-slate-50 dark:bg-slate-950/80 p-4",
            div { class: "mb-3 flex items-center justify-between",
                span { class: "text-sm font-medium text-slate-900 dark:text-slate-200", "Add a Postgres connection" }
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
                            class: "rounded border border-slate-300 dark:border-slate-700 bg-slate-100 dark:bg-slate-950 px-2 py-2 text-sm text-slate-900 dark:text-slate-300",
                            onchange: move |evt| sslmode.set(evt.value()),
                            option { value: "prefer", selected: *sslmode.read() == "prefer", "sslmode: prefer" }
                            option { value: "require", selected: *sslmode.read() == "require", "sslmode: require" }
                            option { value: "disable", selected: *sslmode.read() == "disable", "sslmode: disable" }
                        }
                    }
                }
                input {
                    r#type: "password",
                    class: field_class,
                    placeholder: "password",
                    value: "{password}",
                    oninput: move |evt| password.set(evt.value()),
                }
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
                                    name: "ssh-auth",
                                    checked: !ssh_use_key(),
                                    onchange: move |_| ssh_use_key.set(false),
                                }
                                "ssh-agent"
                            }
                            label { class: "flex items-center gap-2",
                                input {
                                    r#type: "radio",
                                    name: "ssh-auth",
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
                label { class: "flex items-center gap-2 text-xs text-slate-500 dark:text-slate-400",
                    input {
                        r#type: "checkbox",
                        checked: remember(),
                        onchange: move |evt| remember.set(evt.checked()),
                    }
                    "Remember in the system keyring (falls back to this session only)"
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
                        "Connect & save"
                    }
                }
                if let Some(err) = form_error() {
                    p { class: "text-sm text-red-600 dark:text-red-400", "{err}" }
                }
            }
        }
    }
}

/// Fallback display name: "database @ host".
fn default_pg_name(url: &str) -> String {
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
