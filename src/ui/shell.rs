use std::path::PathBuf;
use std::time::Duration;

use dioxus::desktop::tao::dpi::{LogicalSize, PhysicalPosition};
use dioxus::desktop::tao::event::{Event, WindowEvent};
use dioxus::desktop::{use_window, use_wry_event_handler, DesktopService, WindowCloseBehaviour};
use dioxus::prelude::*;
use dioxus_icons::lucide::{Moon, Pencil, Plug, Sun, SunMoon, Trash2, X};

use crate::config::{
    default_settings_path, load_settings, save_window_geometry, BackendKind, SavedConnection,
    Theme, WindowGeometry,
};
use crate::db::{ConnectionId, Dialect};

use super::editor::SqlEditor;
use super::grid::DataGrid;
use super::icons::BackendIcon;
use super::notice::{Banner, BannerKind, EmptyState, Spinner};
use super::schema::SchemaPane;
use super::sidebar::SchemaSidebar;
use super::state::{ActiveView, AppState, ConnectStep};

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
            // Also gated on `any_dirty`: if an in-flight save empties the stage
            // while the prompt is up, it self-dismisses instead of offering to
            // discard nothing.
            if state.confirm_quit.read().to_owned() && state.any_dirty() {
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
                        // Leaves close-behaviour at WindowHides, which is
                        // self-correcting: the next CloseRequested re-sets it
                        // (WindowCloses when clean, WindowHides when still dirty).
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

    // On Windows, the builder's `with_inner_size` treats the native menu bar
    // as part of the requested client area while `inner_size()` reports the
    // content area below it, so the geometry restored by main.rs reads back
    // one menu-height short — and every launch/close cycle would shrink the
    // window by that much (FRE-62). The runtime `set_inner_size` is
    // menu-exclusive like `inner_size()` (verified empirically), so
    // re-applying the requested size once at startup makes the read-back
    // match what was saved. Only fires when a shortfall is actually measured:
    // on platforms where builder and read-back agree nothing happens, and a
    // window manager that *clamped* the size (negative shortfall) is left
    // alone.
    {
        let window = window.clone();
        use_effect(move || {
            if window.is_maximized() {
                return;
            }
            let requested = default_settings_path()
                .map(|path| load_settings(&path).window.unwrap_or_default())
                .unwrap_or_default()
                .sanitized();
            let scale = window.scale_factor();
            let actual = window.inner_size().to_logical::<f64>(scale);
            let (dw, dh) = (
                requested.width - actual.width,
                requested.height - actual.height,
            );
            if (dw > 0.5 || dh > 0.5) && dw >= 0.0 && dh >= 0.0 {
                window.set_inner_size(LogicalSize::new(requested.width, requested.height));
            }
        });
    }

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
            // here while the confirmation is pending. The hide itself emits
            // events (and the 1 s geometry timer is a backstop), so the window
            // bounces straight back. Guarded on visibility so we don't re-raise
            // (and steal focus) on every event once it's already back.
            _ => {
                if *state.confirm_quit.peek() && !window.is_visible() {
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
                        X { size: 16 }
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
                        ShortcutRow { keys: "Ctrl+E", desc: "Cycle Data / SQL / Schema pane" }
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
    let tabs: Vec<(ConnectionId, String, Dialect)> = state
        .registry
        .read()
        .iter()
        .map(|c| (c.id, c.name.clone(), c.pool.dialect()))
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
            for (id, name, dialect) in tabs {
                div {
                    class: if active == ActiveView::Connection(id) {
                        "flex items-center gap-1 rounded-t bg-white dark:bg-slate-900 px-3 py-1.5 text-sm text-slate-900 dark:text-slate-100"
                    } else {
                        "flex items-center gap-1 rounded-t px-3 py-1.5 text-sm text-slate-500 dark:text-slate-400 hover:text-slate-900 dark:hover:text-slate-100"
                    },
                    button {
                        class: "flex items-center gap-1.5",
                        onclick: move |_| state.active.set(ActiveView::Connection(id)),
                        BackendIcon { dialect }
                        "{name}"
                    }
                    button {
                        class: "rounded px-1 py-1 text-slate-500 hover:bg-slate-300 dark:hover:bg-slate-700 hover:text-slate-900 dark:hover:text-slate-200",
                        aria_label: "Close connection",
                        onclick: move |_| state.close_connection(id),
                        X { size: 12 }
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
        Theme::System => rsx! { SunMoon { size: 13 } },
        Theme::Light => rsx! { Sun { size: 13 } },
        Theme::Dark => rsx! { Moon { size: 13 } },
    };
    rsx! {
        button {
            class: "ml-auto flex items-center gap-1 rounded px-2 py-1 text-xs text-slate-500 dark:text-slate-400 hover:bg-slate-200 dark:hover:bg-slate-800 hover:text-slate-900 dark:hover:text-slate-100",
            title: "Theme (click to cycle System / Light / Dark)",
            aria_label: "Toggle theme",
            onclick: move |_| state.set_theme(theme.next()),
            {icon}
            "{theme.label()}"
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
                    PaneButton { id, pane, target: super::state::Pane::Schema, label: "Schema" }
                }
                match pane {
                    super::state::Pane::Sql => rsx! {
                        SqlEditor { key: "sql-{id:?}", id }
                    },
                    // Same selected-table semantics as Data, so the two panes
                    // always describe the same table (FRE-69).
                    super::state::Pane::Schema => match selected {
                        Some(table) => rsx! {
                            SchemaPane { key: "schema-{table.key()}", id, table: table.clone() }
                        },
                        None => rsx! {
                            div { class: "flex flex-1 items-center justify-center",
                                p { class: "text-slate-500", "Select a table to view its schema." }
                            }
                        },
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

/// One segment of the Data / SQL / Schema pane switch.
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
    tunnel: Option<crate::tunnel::TunnelConfig>,
    auth: crate::config::PgAuth,
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
    Edit(SavedConnection),
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
            onmounted: move |evt: MountedEvent| {
                spawn(async move {
                    let _ = evt.set_focus(true).await;
                });
            },
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

/// Launch screen: the persisted saved-connections list plus add flows for
/// SQLite (native file picker) and Postgres (form or URL).
#[component]
fn ConnectionsScreen() -> Element {
    let state = use_context::<AppState>();
    // One open form at a time (they were a mutually-exclusive bool pair
    // before FRE-67); `None` means no modal is up.
    let mut open_form = use_signal(|| Option::<ConnectForm>::None);
    // Locator of the row whose Remove is armed, if any. Removing is instant
    // and unrecoverable (a server entry takes its keyring password with it),
    // so the trash icon only arms the confirmation — one row at a time, since
    // arming another replaces this.
    let mut confirm_remove = use_signal(|| Option::<String>::None);
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
    let saved: Vec<SavedRow> = {
        let open = state.open_locators.read();
        let connecting = state.connecting.read();
        let requests = state.connect_requests.read();
        state
            .saved
            .read()
            .entries()
            .iter()
            .map(|s| {
                let canonical_locator = super::state::saved_open_locator(s);
                let (tunnel, auth) = match s {
                    crate::config::SavedConnection::Postgres { tunnel, auth, .. }
                    | crate::config::SavedConnection::SqlServer { tunnel, auth, .. } => {
                        (tunnel.clone(), auth.clone())
                    }
                    crate::config::SavedConnection::Sqlite { .. } => {
                        (None, crate::config::PgAuth::Password)
                    }
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
                    tunnel,
                    auth,
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
                h1 { class: "text-2xl font-semibold text-slate-900 dark:text-slate-200", "Hubro" }
                if !saved.is_empty() {
                    p { class: "mt-1 text-sm text-slate-500 dark:text-slate-400",
                        "Pick a saved connection, or add another database."
                    }
                }
            }
            if saved.is_empty() {
                // Designed empty state; the Add buttons below are its action.
                EmptyState {
                    icon: rsx! { Plug { size: 40 } },
                    title: "No connections yet",
                    hint: "Add a SQLite file, Postgres server, or SQL Server to get started.",
                }
            }
            if !saved.is_empty() {
                ul { class: "w-full max-w-xl divide-y divide-slate-200 dark:divide-slate-800 rounded border border-slate-300 dark:border-slate-700 bg-slate-50 dark:bg-slate-950/60",
                    for row in saved {
                        // Same row-hover shade as the sidebar's table list. The
                        // Edit/Remove buttons hover one step further so they
                        // stay visible on top of it. Rounding the end rows
                        // keeps the highlight inside the list's rounded border.
                        //
                        // The row's padding lives on the connect button, not on
                        // the li: as li padding it was dead space that lit up on
                        // hover but swallowed the click.
                        li { class: "flex items-stretch first:rounded-t last:rounded-b hover:bg-slate-200 dark:hover:bg-slate-800/60",
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
                                                        open_form.set(Some(ConnectForm::Edit(entry)));
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
                        ConnectForm::Edit(saved @ SavedConnection::SqlServer { .. }) => rsx! {
                            SqlServerForm {
                                edit: saved.clone(),
                                on_done: move |_| open_form.set(None),
                            }
                        },
                        ConnectForm::Edit(saved) => rsx! {
                            PostgresForm {
                                edit: saved.clone(),
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
                    if prompt.backend == BackendKind::SqlServer {
                        state
                            .connect_sqlserver_with_password(
                                prompt.url,
                                prompt.name,
                                entered,
                                remember_choice,
                                prompt.tunnel,
                            )
                            .await;
                    } else {
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
                }
                PromptKind::SshPassphrase => {
                    // An SSH prompt always carries its tunnel config.
                    let Some(tunnel) = prompt.tunnel else { return };
                    if prompt.backend == BackendKind::SqlServer {
                        state
                            .connect_sqlserver_with_ssh_passphrase(
                                prompt.url,
                                prompt.name,
                                tunnel,
                                entered,
                                remember_choice,
                                prompt.auth,
                            )
                            .await;
                    } else {
                        state
                            .connect_postgres_with_ssh_passphrase(
                                prompt.url,
                                prompt.name,
                                tunnel,
                                entered,
                                remember_choice,
                                prompt.auth,
                            )
                            .await;
                    }
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
                    onclick: move |_| {
                        state.clear_pending_edit();
                        state.password_prompt.clone().set(None);
                    },
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
                    onclick: move |_| {
                        state.clear_pending_edit();
                        state.host_key_prompt.clone().set(None);
                    },
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
            if prompt.backend == BackendKind::SqlServer {
                state.connect_sqlserver_with_entra_signin(prompt).await;
            } else {
                state.connect_postgres_with_entra_signin(prompt).await;
            }
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
                    onclick: move |_| {
                        state.clear_pending_edit();
                        state.entra_prompt.clone().set(None);
                    },
                    "Cancel"
                }
            }
        }
    }
}

/// A saved connection's URL split back into the form's individual fields
/// (FRE-75). Both backends share the `scheme://user@host:port/db?opt=…`
/// shape, so one splitter serves both; `option_key` names the query
/// parameter the form's own dropdown owns (`sslmode` / `encrypt`).
struct UrlFields {
    host: String,
    port: String,
    database: String,
    user: String,
    option: Option<String>,
    trust_cert: bool,
}

fn split_url(url: &str, option_key: &str) -> Option<UrlFields> {
    let parsed = url::Url::parse(url).ok()?;
    let mut option = None;
    let mut trust_cert = false;
    // Query keys are compared case-insensitively: the app writes
    // `trustServerCertificate`, and a hand-pasted URL may use any casing.
    let option_key = option_key.to_ascii_lowercase();
    for (key, value) in parsed.query_pairs() {
        let key = key.to_ascii_lowercase();
        if key == option_key {
            option = Some(value.into_owned());
        } else if key == "trustservercertificate" {
            // Same spellings the SQL Server driver accepts.
            trust_cert = matches!(value.to_ascii_lowercase().as_str(), "true" | "yes" | "1");
        }
    }
    Some(UrlFields {
        host: parsed.host_str().unwrap_or_default().to_string(),
        port: parsed.port().map(|p| p.to_string()).unwrap_or_default(),
        database: parsed.path().trim_start_matches('/').to_string(),
        user: percent_decode(parsed.username()),
        option,
        trust_cert,
    })
}

/// Percent-decodes a URL component back to what the user typed into the
/// field (the url crate encodes on the way in).
fn percent_decode(raw: &str) -> String {
    percent_encoding::percent_decode_str(raw)
        .decode_utf8()
        .map(|s| s.into_owned())
        .unwrap_or_else(|_| raw.to_string())
}

/// A saved entry decomposed into the connection forms' field values
/// (FRE-75). Secrets are deliberately absent: the password and SSH
/// passphrase fields always start empty, and an empty field on save means
/// "keep whatever is in the keyring".
#[derive(Clone, PartialEq)]
struct EditPrefill {
    name: String,
    host: String,
    port: String,
    database: String,
    user: String,
    /// `sslmode` for Postgres, `encrypt` for SQL Server.
    option: Option<String>,
    trust_cert: bool,
    auth_mode: String,
    entra_tenant: String,
    entra_client_id: String,
    tunnel: Option<crate::tunnel::TunnelConfig>,
    ssh_host: String,
    ssh_port: String,
    ssh_user: String,
    ssh_use_key: bool,
    ssh_key_path: String,
}

impl Default for EditPrefill {
    /// The add flow's starting point. Note `auth_mode`/`entra_tenant` are
    /// the forms' real defaults rather than empty strings — the password
    /// field only renders while `auth_mode` is "password".
    fn default() -> Self {
        EditPrefill {
            name: String::new(),
            host: String::new(),
            port: String::new(),
            database: String::new(),
            user: String::new(),
            option: None,
            trust_cert: false,
            auth_mode: "password".to_string(),
            entra_tenant: "organizations".to_string(),
            entra_client_id: String::new(),
            tunnel: None,
            ssh_host: String::new(),
            ssh_port: String::new(),
            ssh_user: String::new(),
            ssh_use_key: false,
            ssh_key_path: String::new(),
        }
    }
}

impl EditPrefill {
    fn from_saved(saved: SavedConnection) -> Self {
        use crate::azure::EntraAuth;
        use crate::config::PgAuth;
        use crate::tunnel::TunnelAuth;
        let (name, url, tunnel, auth, option_key) = match saved {
            SavedConnection::Postgres {
                name,
                url,
                tunnel,
                auth,
            } => (name, url, tunnel, auth, "sslmode"),
            SavedConnection::SqlServer {
                name,
                url,
                tunnel,
                auth,
            } => (name, url, tunnel, auth, "encrypt"),
            // SQLite entries carry only a path; they have no edit form.
            SavedConnection::Sqlite { name, .. } => {
                return EditPrefill {
                    name,
                    ..EditPrefill::default()
                }
            }
        };
        let fields = split_url(&url, option_key);
        let (auth_mode, entra_tenant, entra_client_id) = match auth {
            PgAuth::Password => ("password".into(), "organizations".into(), String::new()),
            PgAuth::Entra(EntraAuth::Interactive { tenant, client_id }) => (
                "entra-interactive".to_string(),
                tenant,
                client_id.unwrap_or_default(),
            ),
            PgAuth::Entra(EntraAuth::ManagedIdentity { client_id }) => (
                "entra-mi".to_string(),
                "organizations".to_string(),
                client_id.unwrap_or_default(),
            ),
        };
        let (ssh_use_key, ssh_key_path) = match tunnel.as_ref().map(|t| &t.auth) {
            Some(TunnelAuth::KeyFile { path }) => (true, path.display().to_string()),
            _ => (false, String::new()),
        };
        EditPrefill {
            name,
            host: fields.as_ref().map(|f| f.host.clone()).unwrap_or_default(),
            port: fields.as_ref().map(|f| f.port.clone()).unwrap_or_default(),
            database: fields
                .as_ref()
                .map(|f| f.database.clone())
                .unwrap_or_default(),
            user: fields.as_ref().map(|f| f.user.clone()).unwrap_or_default(),
            option: fields.as_ref().and_then(|f| f.option.clone()),
            trust_cert: fields.as_ref().is_some_and(|f| f.trust_cert),
            auth_mode,
            entra_tenant,
            entra_client_id,
            ssh_host: tunnel.as_ref().map(|t| t.host.clone()).unwrap_or_default(),
            ssh_port: tunnel
                .as_ref()
                .map(|t| t.port.to_string())
                .unwrap_or_default(),
            ssh_user: tunnel.as_ref().map(|t| t.user.clone()).unwrap_or_default(),
            ssh_use_key,
            ssh_key_path,
            tunnel,
        }
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
    use crate::config::PgAuth;
    use crate::tunnel::{TunnelAuth, TunnelConfig};
    let state = use_context::<AppState>();
    let prefill = edit.clone().map(EditPrefill::from_saved);
    let seed = prefill.clone().unwrap_or_default();
    let old_locator = edit.as_ref().map(|e| e.locator().to_string());
    let editing = edit.is_some();
    let mut use_url = use_signal(|| false);
    let mut name = use_signal(|| seed.name.clone());
    let mut host = use_signal(|| seed.host.clone());
    let mut port = use_signal(|| seed.port.clone());
    let mut database = use_signal(|| seed.database.clone());
    let mut user = use_signal(|| seed.user.clone());
    // Never prefilled: the stored secret isn't shown, and an empty field on
    // save means "keep the existing keyring entry" (FRE-75).
    let mut password = use_signal(String::new);
    let mut remember = use_signal(|| true);
    let mut sslmode = use_signal(|| seed.option.clone().unwrap_or_else(|| "prefer".to_string()));
    // Authentication: "password" (default), "entra-interactive", or "entra-mi".
    let auth_mode = use_signal(|| seed.auth_mode.clone());
    let entra_tenant = use_signal(|| seed.entra_tenant.clone());
    let entra_client_id = use_signal(|| seed.entra_client_id.clone());
    let mut pasted_url = use_signal(String::new);
    let use_tunnel = use_signal(|| seed.tunnel.is_some());
    let ssh_host = use_signal(|| seed.ssh_host.clone());
    let ssh_port = use_signal(|| seed.ssh_port.clone());
    let ssh_user = use_signal(|| seed.ssh_user.clone());
    // false = ssh-agent (the default), true = key file.
    let ssh_use_key = use_signal(|| seed.ssh_use_key);
    let ssh_key_path = use_signal(|| seed.ssh_key_path.clone());
    let ssh_passphrase = use_signal(String::new);
    let mut form_error = use_signal(|| Option::<String>::None);

    let mut submit = move || {
        // Cloned per attempt: the async block below takes ownership, and
        // `submit` has to stay FnMut.
        let old_locator = old_locator.clone();
        // Tunnel settings are validated first so a bad SSH field fails
        // before any connect attempt.
        let tunnel: Option<TunnelConfig> = match tunnel_from_form(
            *use_tunnel.peek(),
            &ssh_host.peek(),
            &ssh_port.peek(),
            &ssh_user.peek(),
            *ssh_use_key.peek(),
            &ssh_key_path.peek(),
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
                default_server_name(&url)
            } else {
                entered
            }
        };
        // Authentication mode from the selector (FRE-44). Entra takes the
        // `user` field as the Entra principal; a token replaces the password.
        let auth: PgAuth = match auth_from_form(
            &auth_mode.peek(),
            &entra_tenant.peek(),
            &entra_client_id.peek(),
        ) {
            Ok(auth) => auth,
            Err(err) => {
                form_error.set(Some(err));
                return;
            }
        };
        if matches!(auth, PgAuth::Entra(_)) && entra_principal_missing(&url) {
            form_error.set(Some(
                "Entra needs a principal — fill the user field (e.g. you@contoso.com)".to_string(),
            ));
            return;
        }
        let mut entered_password = password.peek().clone();
        if entered_password.is_empty() {
            if let Some(embedded) = embedded_password {
                entered_password = embedded;
            }
        }
        let remember_choice = *remember.peek();
        form_error.set(None);
        // spawn_forever: closing the modal (X or Escape) unmounts this form,
        // and a scope-tied `spawn` would be cancelled at its next await —
        // abandoning the connect with its reservation still held, which
        // leaves the row stuck showing progress it can no longer cancel.
        dioxus::core::spawn_forever(async move {
            // An edit is saved by whichever path confirms the connect —
            // including the Entra sign-in card, which resolves after this
            // form has closed — so the intent is registered up front rather
            // than applied here (FRE-75).
            if let Some(old) = &old_locator {
                state.set_pending_edit(old.clone(), url.clone());
            }
            // "Leave the password empty to keep the existing one" has to keep
            // working when the edit moves the locator: the stored secret is
            // still filed under the OLD one, and the connect below looks up
            // the new. Carry it over first (FRE-75).
            if entered_password.is_empty() {
                if let Some(old) = old_locator.as_ref().filter(|old| **old != url) {
                    if let Ok(Some(stored)) = crate::secrets::get_password_async(old.clone()).await
                    {
                        entered_password = stored;
                    }
                }
            }
            // An entered passphrase seeds session memory so the tunnel open
            // finds it, exactly as if it came from the prompt.
            if let Some(passphrase) = &entered_passphrase {
                state.stash_ssh_passphrase(&url, passphrase.clone());
            }
            if matches!(auth, PgAuth::Entra(_)) {
                // Entra: the connect either succeeds silently (and the flow
                // saves it) or raises the sign-in card (which saves on
                // completion). Either way, close the form — the card takes over.
                //
                // Closed *before* the await, not after: `on_done` is owned by
                // the connections screen, and a successful connect switches to
                // the new tab and unmounts it. This task outlives that (it is
                // root-spawned), so calling the handler afterwards would touch
                // a dropped scope's storage and panic. The Entra path yields
                // after opening the tab (it caches the refresh token), so the
                // unmount really does land first.
                //
                // Guarded because ordering alone is not a guarantee: a *second*
                // focus-taking connect finishing mid-submit can unmount the
                // screen at any await here. The screen is mounted exactly when
                // this view is active, and when it is not, closing the form is
                // a no-op anyway — it went with the screen.
                close_form(&state, &on_done);
                state
                    .connect_postgres(url.clone(), display_name.clone(), tunnel.clone(), auth)
                    .await;
                return;
            }
            if entered_password.is_empty() {
                state
                    .connect_postgres(
                        url.clone(),
                        display_name.clone(),
                        tunnel.clone(),
                        PgAuth::Password,
                    )
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
            // Only save and close the form when the connection worked. Closing
            // comes first for the reason above: `persist_ssh_passphrase` awaits,
            // and by the time it returns the connections screen that owns
            // `on_done` is gone. Everything after it is on `AppState`, whose
            // signals are root-owned and outlive this task.
            if state.open_locators.peek().iter().any(|(_, l)| *l == url) {
                close_form(&state, &on_done);
                if remember_choice && entered_passphrase.is_some() {
                    state.persist_ssh_passphrase(&url).await;
                }
                state.add_saved_postgres(display_name, url, tunnel, PgAuth::Password);
            }
        });
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
    use crate::config::PgAuth;
    use crate::tunnel::{TunnelAuth, TunnelConfig};
    let state = use_context::<AppState>();
    let seed = edit
        .clone()
        .map(EditPrefill::from_saved)
        .unwrap_or_default();
    let old_locator = edit.as_ref().map(|e| e.locator().to_string());
    let editing = edit.is_some();
    let mut use_url = use_signal(|| false);
    let mut name = use_signal(|| seed.name.clone());
    let mut host = use_signal(|| seed.host.clone());
    let mut port = use_signal(|| seed.port.clone());
    let mut database = use_signal(|| seed.database.clone());
    let mut user = use_signal(|| seed.user.clone());
    // Never prefilled: an empty field on save keeps the keyring entry.
    let mut password = use_signal(String::new);
    let mut remember = use_signal(|| true);
    // Matches the URL params the backend accepts: encrypt=on|off|plaintext.
    let mut encrypt = use_signal(|| seed.option.clone().unwrap_or_else(|| "on".to_string()));
    // Accept the server's TLS certificate without CA validation — needed for
    // dev servers with self-signed certs (e.g. the stock Docker image).
    let mut trust_cert = use_signal(|| seed.trust_cert);
    // Authentication: "password" (default), "entra-interactive", or "entra-mi".
    let auth_mode = use_signal(|| seed.auth_mode.clone());
    let entra_tenant = use_signal(|| seed.entra_tenant.clone());
    let entra_client_id = use_signal(|| seed.entra_client_id.clone());
    let mut pasted_url = use_signal(String::new);
    let use_tunnel = use_signal(|| seed.tunnel.is_some());
    let ssh_host = use_signal(|| seed.ssh_host.clone());
    let ssh_port = use_signal(|| seed.ssh_port.clone());
    let ssh_user = use_signal(|| seed.ssh_user.clone());
    // false = ssh-agent (the default), true = key file.
    let ssh_use_key = use_signal(|| seed.ssh_use_key);
    let ssh_key_path = use_signal(|| seed.ssh_key_path.clone());
    let ssh_passphrase = use_signal(String::new);
    let mut form_error = use_signal(|| Option::<String>::None);

    let mut submit = move || {
        // Cloned per attempt: the async block below takes ownership, and
        // `submit` has to stay FnMut.
        let old_locator = old_locator.clone();
        // Tunnel settings are validated first so a bad SSH field fails
        // before any connect attempt.
        let tunnel: Option<TunnelConfig> = match tunnel_from_form(
            *use_tunnel.peek(),
            &ssh_host.peek(),
            &ssh_port.peek(),
            &ssh_user.peek(),
            *ssh_use_key.peek(),
            &ssh_key_path.peek(),
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
                default_server_name(&url)
            } else {
                entered
            }
        };
        // Authentication mode from the selector (FRE-58, mirroring FRE-44).
        // Entra takes the user field as the principal; a token replaces the
        // password.
        let auth: PgAuth = match auth_from_form(
            &auth_mode.peek(),
            &entra_tenant.peek(),
            &entra_client_id.peek(),
        ) {
            Ok(auth) => auth,
            Err(err) => {
                form_error.set(Some(err));
                return;
            }
        };
        if matches!(auth, PgAuth::Entra(_)) && entra_principal_missing(&url) {
            form_error.set(Some(
                "Entra needs a principal — fill the user field (e.g. you@contoso.com)".to_string(),
            ));
            return;
        }
        let mut entered_password = password.peek().clone();
        if entered_password.is_empty() {
            if let Some(embedded) = embedded_password {
                entered_password = embedded;
            }
        }
        let remember_choice = *remember.peek();
        form_error.set(None);
        // spawn_forever: closing the modal (X or Escape) unmounts this form,
        // and a scope-tied `spawn` would be cancelled at its next await —
        // abandoning the connect with its reservation still held, which
        // leaves the row stuck showing progress it can no longer cancel.
        dioxus::core::spawn_forever(async move {
            // An edit is saved by whichever path confirms the connect —
            // including the Entra sign-in card, which resolves after this
            // form has closed — so the intent is registered up front rather
            // than applied here (FRE-75).
            if let Some(old) = &old_locator {
                state.set_pending_edit(old.clone(), url.clone());
            }
            // "Leave the password empty to keep the existing one" has to keep
            // working when the edit moves the locator: the stored secret is
            // still filed under the OLD one, and the connect below looks up
            // the new. Carry it over first (FRE-75).
            if entered_password.is_empty() {
                if let Some(old) = old_locator.as_ref().filter(|old| **old != url) {
                    if let Ok(Some(stored)) = crate::secrets::get_password_async(old.clone()).await
                    {
                        entered_password = stored;
                    }
                }
            }
            // An entered passphrase seeds session memory so the tunnel open
            // finds it, exactly as if it came from the prompt.
            if let Some(passphrase) = &entered_passphrase {
                state.stash_ssh_passphrase(&url, passphrase.clone());
            }
            if matches!(auth, PgAuth::Entra(_)) {
                // Entra: the connect either succeeds silently (and the flow
                // saves it) or raises the sign-in card (which saves on
                // completion). Either way, close the form — the card takes over.
                // Closed before the await, for the same reason as the Postgres
                // form: a successful connect unmounts the screen that owns
                // `on_done`, and this task outlives it.
                close_form(&state, &on_done);
                state
                    .connect_sqlserver(url.clone(), display_name.clone(), tunnel.clone(), auth)
                    .await;
                return;
            }
            if entered_password.is_empty() {
                // No password entered: try as-is (the keyring may hold one);
                // an auth failure raises the password prompt.
                state
                    .connect_sqlserver(
                        url.clone(),
                        display_name.clone(),
                        tunnel.clone(),
                        PgAuth::Password,
                    )
                    .await;
            } else {
                state
                    .connect_sqlserver_with_password(
                        url.clone(),
                        display_name.clone(),
                        entered_password,
                        remember_choice,
                        tunnel.clone(),
                    )
                    .await;
            }
            // The connect flow saves on success; only close the form then —
            // and close before `persist_ssh_passphrase` awaits, since the
            // screen owning `on_done` is gone once the new tab is up.
            if state.open_locators.peek().iter().any(|(_, l)| *l == url) {
                close_form(&state, &on_done);
                if remember_choice && entered_passphrase.is_some() {
                    state.persist_ssh_passphrase(&url).await;
                }
            }
        });
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
fn auth_from_form(
    mode: &str,
    tenant: &str,
    client_id: &str,
) -> Result<crate::config::PgAuth, String> {
    use crate::azure::EntraAuth;
    use crate::config::PgAuth;
    let client = client_id.trim().to_string();
    match mode {
        "entra-interactive" => {
            let tenant = tenant.trim().to_string();
            if tenant.is_empty() {
                return Err("the Entra tenant must not be empty".to_string());
            }
            Ok(PgAuth::Entra(EntraAuth::Interactive {
                tenant,
                client_id: (!client.is_empty()).then_some(client),
            }))
        }
        "entra-mi" => Ok(PgAuth::Entra(EntraAuth::ManagedIdentity {
            client_id: (!client.is_empty()).then_some(client),
        })),
        _ => Ok(PgAuth::Password),
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
