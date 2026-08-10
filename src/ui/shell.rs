use std::time::Duration;

use dioxus::desktop::tao::dpi::{LogicalSize, PhysicalPosition};
use dioxus::desktop::tao::event::{Event, WindowEvent};
use dioxus::desktop::{use_window, use_wry_event_handler, DesktopService, WindowCloseBehaviour};
use dioxus::prelude::*;
use dioxus_icons::lucide::{Moon, Sun, SunMoon, X};

use crate::cli::Startup;
use crate::config::{
    default_settings_path, load_settings, save_window_geometry, ConnectionColor, Theme,
    WindowGeometry,
};
use crate::db::{ConnectionId, Dialect, WriteProtection};

use super::connections::ConnectionsScreen;
use super::editor::SqlEditor;
use super::grid::DataGrid;
use super::icons::BackendIcon;
use super::schema::SchemaPane;
use super::sidebar::SchemaSidebar;
use super::state::{ActiveView, AppState};

/// The window-level keyboard listener (FRE-15). Installed once as a plain
/// `keydown` handler on `window` — a webview-robust way to capture app-global
/// shortcuts regardless of which element holds focus (a focusable Dioxus
/// wrapper would only see keys while it, and not the sidebar/grid/buttons,
/// had focus). It self-guards against text-entry contexts (inputs, the cell
/// editor, CodeMirror) so typing never triggers a shortcut, handles the
/// focus-only shortcuts entirely in JS (focus the grid filter, focus the
/// schema filter, focus + arrow through the sidebar table list), and forwards
/// the state-changing ones to Rust via `dioxus.send`. The `__dvKeys` guard
/// makes a re-install (e.g. dev hot-reload) a no-op.
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
    // Ctrl+F focuses the schema sidebar's filter (FRE-107) and selects what
    // is already there, so a second search replaces the first by typing.
    // preventDefault also suppresses the webview's own find-in-page.
    if (e.ctrlKey && (e.key === 'f' || e.key === 'F')) {
      const f = document.getElementById('dv-schema-filter');
      if (f) { e.preventDefault(); f.focus(); f.select(); }
      return;
    }
    if (e.ctrlKey && (e.key === 'e' || e.key === 'E')) { e.preventDefault(); dioxus.send('pane'); return; }
    // Ctrl+D docks/undocks the row detail panel beside the grid (FRE-109).
    if (e.ctrlKey && (e.key === 'd' || e.key === 'D')) { e.preventDefault(); dioxus.send('rowdetail'); return; }
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
                    "rowdetail" => state.toggle_row_detail(),
                    _ => {}
                }
            }
        });
    });

    // Restore the previous session exactly once, from this component's scope
    // (not a root `spawn_forever` in AppState::new): restore drives the normal
    // connect flow, which writes the core connection signals — running it here
    // keeps those writes in a live scope, matching the manual connect path.
    //
    // A database named on the command line (FRE-114) opens *after* the
    // restore, in the same task: restore ends by setting the active view to
    // whichever tab was in front last time, so opening first would leave the
    // database the user just asked for behind a tab they didn't.
    use_hook(|| {
        let startup = try_consume_context::<Startup>().and_then(|Startup(target)| target);
        spawn(async move {
            state.restore_session().await;
            if let Some(target) = startup {
                state.open_target(target).await;
            }
        });
    });

    // Databases the OS hands to the *running* app: on macOS a double-clicked
    // file arrives as an `open` Apple Event rather than in argv (FRE-114).
    // Claimed once, and the queue is unbounded, so an event delivered before
    // this task starts is still opened rather than dropped.
    use_hook(|| {
        let Some(mut opened) = crate::cli::take_opened() else {
            return;
        };
        spawn(async move {
            while let Some(target) = opened.recv().await {
                state.open_target(target).await;
            }
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
                        ShortcutRow { keys: "Esc", desc: "Close the popup / collapse the selection" }
                        ShortcutRow { keys: "Ctrl+D", desc: "Toggle the row detail panel" }
                    }
                    ShortcutGroup { title: "Cell selection",
                        ShortcutRow { keys: "Shift+↑ ↓ ← →", desc: "Extend the selection" }
                        ShortcutRow { keys: "Shift+click", desc: "Extend the selection to a cell" }
                        ShortcutRow { keys: "Shift+Space", desc: "Select the whole row" }
                        ShortcutRow { keys: "Ctrl+Space", desc: "Select the whole column" }
                        ShortcutRow { keys: "Shift+click header", desc: "Select the whole column" }
                        ShortcutRow { keys: "Ctrl+A", desc: "Select the whole page" }
                        ShortcutRow { keys: "Ctrl+C", desc: "Copy (TSV; one cell copies its value)" }
                    }
                    ShortcutGroup { title: "Navigation",
                        ShortcutRow { keys: "/", desc: "Focus the grid's filter box" }
                        ShortcutRow { keys: "Ctrl+F", desc: "Filter the table list (Esc clears)" }
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

/// One tab's render data, snapshotted out of the registry so the loop can
/// hand owned values to event handlers.
struct TabEntry {
    id: ConnectionId,
    name: String,
    dialect: Dialect,
    /// Accent colour and write protection (FRE-111).
    color: Option<ConnectionColor>,
    protection: WriteProtection,
}

/// One tab per open connection, plus a fixed tab for the connections screen.
#[component]
fn TabBar() -> Element {
    let mut state = use_context::<AppState>();
    let active = *state.active.read();
    // Owned copies so the loop can hand ids/names to event handlers.
    let tabs: Vec<TabEntry> = {
        let colors = state.connection_colors.read();
        state
            .registry
            .read()
            .iter()
            .map(|c| TabEntry {
                id: c.id,
                name: c.name.clone(),
                dialect: c.pool.dialect(),
                color: colors.get(&c.id).copied(),
                protection: c.protection,
            })
            .collect()
    };
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
            for tab in tabs {
                div {
                    key: "{tab.id:?}",
                    class: if active == ActiveView::Connection(tab.id) {
                        "flex items-center gap-1 rounded-t bg-white dark:bg-slate-900 px-3 py-1.5 text-sm text-slate-900 dark:text-slate-100"
                    } else {
                        "flex items-center gap-1 rounded-t px-3 py-1.5 text-sm text-slate-500 dark:text-slate-400 hover:text-slate-900 dark:hover:text-slate-100"
                    },
                    // The accent rides the tab's top edge (FRE-111) so it
                    // stays visible on the inactive tabs too, where a
                    // background tint would be lost against the bar.
                    border_top: if let Some(color) = tab.color { "2px solid {color.css()}" },
                    padding_top: if tab.color.is_some() { "4px" },
                    button {
                        class: "flex items-center gap-1.5",
                        onclick: {
                            let id = tab.id;
                            move |_| state.active.set(ActiveView::Connection(id))
                        },
                        BackendIcon { dialect: tab.dialect }
                        "{tab.name}"
                        // A protected tab says so wherever it is seen, not
                        // only in the connections list.
                        if let Some(badge) = tab.protection.badge() {
                            span {
                                class: "rounded bg-amber-100 dark:bg-amber-900/50 px-1 py-0.5 text-[10px] leading-none text-amber-700 dark:text-amber-300",
                                title: "{badge}",
                                if tab.protection == WriteProtection::ReadOnly { "RO" } else { "!" }
                            }
                        }
                    }
                    button {
                        class: "rounded px-1 py-1 text-slate-500 hover:bg-slate-300 dark:hover:bg-slate-700 hover:text-slate-900 dark:hover:text-slate-200",
                        aria_label: "Close connection",
                        onclick: {
                            let id = tab.id;
                            move |_| state.close_connection(id)
                        },
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
