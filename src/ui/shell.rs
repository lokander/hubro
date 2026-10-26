use std::time::Duration;

use dioxus::desktop::tao::dpi::{LogicalSize, PhysicalPosition};
use dioxus::desktop::tao::event::{Event, WindowEvent};
use dioxus::desktop::{use_window, use_wry_event_handler, DesktopService, WindowCloseBehaviour};
use dioxus::prelude::*;
use dioxus_icons::lucide::{Moon, Sun, SunMoon, X};

use crate::cli::Startup;
use crate::config::{load_settings, save_window_geometry, ConnectionColor, Theme, WindowGeometry};
use crate::db::{ConnectionId, Dialect, WriteProtection};
use crate::ui::classes::{BTN_GHOST, BTN_PRIMARY};

use super::connections::ConnectionsScreen;
use super::editor::SqlEditor;
use super::grid::DataGrid;
use super::icons::BackendIcon;
use super::notice::{
    a_held_key_is_not_a_press, Banner, BannerKind, Gesture, Overlay, OverlayAlign,
};
use super::schema::SchemaPane;
use super::schema_edit::SchemaEditLine;
use super::sidebar::SchemaSidebar;
use super::state::{ActiveView, AppState, SettingsNotice};

/// The window-level keyboard listener. Installed once as a plain
/// `keydown` handler on `window` — a webview-robust way to capture app-global
/// shortcuts regardless of which element holds focus (a focusable Dioxus
/// wrapper would only see keys while it, and not the sidebar/grid/buttons,
/// had focus). It self-guards against text-entry contexts (inputs, the cell
/// editor, CodeMirror) so typing never triggers a shortcut, handles the
/// focus-only shortcuts entirely in JS (focus the grid filter, focus the
/// schema filter, focus + arrow through the sidebar table list), and forwards
/// the state-changing ones to Rust via `dioxus.send`. While a modal overlay
/// is open it stands aside for everything but `?`. The `__dvKeys` guard
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
  // Ctrl or Cmd: the rule every Ctrl shortcut follows (see `accel_held`).
  const accel = (e) => e.ctrlKey || e.metaKey;
  const tableButtons = () => Array.from(document.querySelectorAll('.dv-table-btn'));
  // While a modal overlay is open, only `?` (the cheatsheet, which opens
  // over it) is a window shortcut. The rest act on what is behind the
  // overlay, and `/` or Ctrl+F would move focus there, out of the overlay's
  // reach of Escape.
  const underModal = (e) => e.key !== '?' && !!document.querySelector('[data-dv-modal]');
  window.addEventListener('keydown', (e) => {
    if (underModal(e)) return;
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
    if (e.key === '/') {
      const f = document.getElementById('dv-filter');
      if (f) { e.preventDefault(); f.focus(); }
      return;
    }
    // Ctrl+F focuses the schema sidebar's filter and selects what
    // is already there, so a second search replaces the first by typing.
    // preventDefault also suppresses the webview's own find-in-page.
    if (accel(e) && (e.key === 'f' || e.key === 'F')) {
      const f = document.getElementById('dv-schema-filter');
      if (f) { e.preventDefault(); f.focus(); f.select(); }
      return;
    }
    // A held Ctrl+E says so: its repeats must never confirm the
    // unsaved-changes guard (see `Gesture`).
    if (accel(e) && (e.key === 'e' || e.key === 'E')) { e.preventDefault(); dioxus.send(e.repeat ? 'pane-repeat' : 'pane'); return; }
    // Ctrl+D docks/undocks the row detail panel beside the grid.
    if (accel(e) && (e.key === 'd' || e.key === 'D')) { e.preventDefault(); dioxus.send('rowdetail'); return; }
    if (accel(e) && (e.key === 'b' || e.key === 'B')) {
      e.preventDefault();
      const btns = tableButtons();
      const cur = btns.find((b) => b.getAttribute('data-selected') === 'true') || btns[0];
      if (cur) cur.focus();
      return;
    }
  });
})();
"#;

/// The gesture behind the pane shortcut's message from [`GLOBAL_KEYS_JS`],
/// which sends `pane-repeat` for a held Ctrl+E's auto-repeats. Leaving the
/// data pane with staged edits takes a second, confirming Ctrl+E (the
/// unsaved-changes guard), and a held key's repeat must never be that press —
/// a 1.5 s held Ctrl+E used to discard the edits.
fn pane_shortcut(msg: &str) -> Gesture {
    Gesture::key(msg == "pane-repeat")
}

/// Whether a key event's modifiers count as "Ctrl" for the app's shortcuts:
/// Ctrl or Cmd (⌘), on every platform.
///
/// One rule rather than one per OS: a macOS user reaches for Cmd and everyone
/// else for Ctrl, and neither combination means anything else to the app.
/// Elsewhere Cmd is the Super/Windows key, which the desktop usually keeps for
/// itself, so accepting it there costs nothing. [`GLOBAL_KEYS_JS`]'s `accel`
/// is the same rule for the window-level shortcuts, and the cheatsheet states
/// it. The SQL editor is the one exception: CodeMirror binds Run to Ctrl+Enter,
/// or Cmd+Enter on macOS.
pub(crate) fn accel_held(modifiers: Modifiers) -> bool {
    modifiers.ctrl() || modifiers.meta()
}

/// The cheatsheet's statement of [`accel_held`]'s rule.
const ACCEL_NOTE: &str =
    "Ctrl also means ⌘ Cmd: every Ctrl shortcut takes either, except the SQL editor's.";

/// Top-level layout: tab bar over the active view.
#[component]
pub fn Shell() -> Element {
    let state = use_context::<AppState>();
    let active = *state.active.read();
    let dark = state.dark;
    let show_cheatsheet = state.show_cheatsheet;
    let settings_notice = match &*state.settings_notice.read() {
        SettingsNotice::Showing(_, message) => Some(message.clone()),
        SettingsNotice::Quiet | SettingsNotice::Dismissed(_) => None,
    };

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
                    // A held Ctrl+E repeats, and says so: see `pane_shortcut`.
                    "pane" | "pane-repeat" => state.toggle_active_pane(pane_shortcut(&msg)),
                    "rowdetail" => state.toggle_row_detail(),
                    _ => {}
                }
            }
        });
    });

    // Restore the previous session exactly once, from this component's scope
    // rather than a root `spawn_forever` in AppState::new: restore drives the
    // normal connect flow, so it belongs beside the manual connect path it
    // mirrors. (It used to be justified by signal scope as well; that reason
    // went away when every AppState signal became root-scoped.)
    //
    // The same task then opens every database hubro was *told* to open:
    // the command-line target, then the ones the OS hands over
    // (on macOS a double-clicked file arrives as an `open` Apple Event rather
    // than in argv; the queue is unbounded, so one delivered before the first
    // render is opened rather than dropped).
    //
    // `restore_session().await` returns once the restored tabs' connects have
    // *started*; they run in the background, each cancellable from its row,
    // and what follows does not wait for them — a restored tab whose server
    // never answers used to hold the command-line target up behind it. The
    // targets themselves still open one at a time, in order:
    // `next_startup_target` holds that ordering, and its tests hold it.
    //
    // Running beside the restore cannot cost a target the foreground: the
    // restored tab that was in front last time comes forward only while the
    // connections screen is still showing (`restored_tab_takes_focus`), and a
    // target the user asked for has replaced it by then.
    use_hook(|| {
        let mut startup = try_consume_context::<Startup>().and_then(|Startup(target)| target);
        let mut opened = crate::cli::take_opened();
        spawn(async move {
            state.restore_session().await;
            while let Some(target) =
                crate::cli::next_startup_target(&mut startup, &mut opened).await
            {
                state.open_target(target).await;
            }
        });
    });

    // Session persistence: re-snapshot whenever the open tabs, their
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
            // Above every view: a preference that does not stick is about
            // the whole app, not whichever screen is showing.
            if let Some(message) = settings_notice {
                div { class: "border-b border-slate-200 dark:border-slate-800 px-3 py-2",
                    Banner {
                        kind: BannerKind::Warning,
                        message,
                        on_dismiss: move |_| state.dismiss_settings_notice(),
                    }
                }
            }
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
            // After the cheatsheet, so it is drawn over it: the two share a
            // z-level, so the later one is on top.
            if state.quit_confirm_showing() {
                QuitConfirmDialog {}
            }
        }
    }
}

/// Confirmation shown when the user tries to close the window with unsaved
/// staged edits. The close was vetoed (the window switched to
/// hide-on-close); here the user either discards and quits for real or cancels
/// back into the app to save.
#[component]
fn QuitConfirmDialog() -> Element {
    let mut state = use_context::<AppState>();
    let window = use_window();
    rsx! {
        // Escape cancels, the same as the Cancel button: it keeps the edits
        // and the window. A backdrop click does not — a decision about
        // unsaved work is not one to make by missing a button.
        Overlay {
            on_close: move |_| state.confirm_quit.set(false),
            dismiss_on_backdrop: false,
            align: OverlayAlign::Center,
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
                        class: "{BTN_PRIMARY} bg-rose-600 hover:bg-rose-500",
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

/// Captures window size/position and persists it to settings.toml so
/// the next launch restores it. Renders nothing.
///
/// Mechanism: a `use_wry_event_handler` updates an in-memory geometry on every
/// `Resized`/`Moved` (cheap, no file I/O) and writes the final value on
/// `CloseRequested`; a 1 s poll writes it when it settles, so dragging the
/// window produces one write, not one per pixel. Writes are best-effort — a
/// failure only means the geometry won't survive this restart, never a crash
/// — but not silent: the first one raises the settings notice
/// ([`AppState::report_settings_write`]).
#[component]
fn WindowPersistence() -> Element {
    let mut state = use_context::<AppState>();
    let window = use_window();
    let mut latest = use_signal(|| read_geometry(&window));

    // On Windows, the builder's `with_inner_size` treats the native menu bar
    // as part of the requested client area while `inner_size()` reports the
    // content area below it, so the geometry restored by main.rs reads back
    // one menu-height short — and every launch/close cycle would shrink the
    // window by that much. The runtime `set_inner_size` is
    // menu-exclusive like `inner_size()` (verified empirically), so
    // re-applying the requested size once at startup makes the read-back
    // match what was saved. Only fires when a shortfall is actually measured:
    // on platforms where builder and read-back agree nothing happens, and a
    // window manager that *clamped* the size (negative shortfall) is left
    // alone.
    //
    // Two known limits, both left as they are until they bite:
    // - A window launched maximized skips the correction, so un-maximizing
    //   returns to the builder's menu-inclusive size and that session still
    //   loses one menu-height on close. Applying the correction on the first
    //   un-maximize, in the `Resized` handler, would close it.
    // - The gate needs both axes non-negative, so a sub-pixel negative on one
    //   axis (possible for one session after a DPI change) also skips the
    //   correction on the other. The next launch corrects again, but the
    //   height lost that session stays lost; applying the correction per axis
    //   would close it.
    {
        let window = window.clone();
        use_effect(move || {
            if window.is_maximized() {
                return;
            }
            let requested = state
                .paths
                .peek()
                .settings
                .clone()
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
                if let Some(path) = state.paths.peek().settings.clone() {
                    state.report_settings_write(
                        "window geometry",
                        save_window_geometry(&path, latest.peek().sanitized()),
                    );
                }
                // This handler runs (via `app.tick`) *before* the built-in
                // close handler reads the close behaviour, so setting it here
                // decides what that close does. With unsaved edits, switch to
                // hide-on-close to veto the destroy and raise the confirmation
                // (the `_` arm below re-shows the just-hidden window).
                if state.any_dirty() {
                    window.set_close_behavior(WindowCloseBehaviour::WindowHides);
                    state.confirm_quit.set(true);
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
                if let Some(path) = state.paths.peek().settings.clone() {
                    state.report_settings_write(
                        "window geometry",
                        save_window_geometry(&path, current.sanitized()),
                    );
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

/// The `?` shortcut cheatsheet: a modal overlay grouping every keybinding,
/// dismissed by Escape, a click on the backdrop, or `?` again. Kept in sync
/// by hand with the actual bindings in [`GLOBAL_KEYS_JS`] and the grid's key
/// handler.
#[component]
fn Cheatsheet() -> Element {
    let state = use_context::<AppState>();
    rsx! {
        Overlay {
            on_close: move |_| state.close_cheatsheet(),
            dismiss_on_backdrop: true,
            align: OverlayAlign::Center,
            div {
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
                        ShortcutRow { keys: "Ctrl+Enter", desc: "Run the buffer or selection (⌘+Enter on macOS)" }
                    }
                }
                p { class: "mt-4 text-xs text-slate-500 dark:text-slate-400",
                    "{ACCEL_NOTE}"
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
    /// Accent colour and write protection.
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
                    // The accent rides the tab's top edge so it
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
                        onkeydown: a_held_key_is_not_a_press,
                        onclick: {
                            let id = tab.id;
                            move |_| state.close_connection(id, Gesture::CLICK)
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
            class: "ml-auto flex items-center gap-1 {BTN_GHOST}",
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
    // Its SSH session ended under it: say so above every pane, since every
    // pane's own errors would otherwise be all there is to go on.
    let lost = state.lost_tunnels.read().get(&id).cloned();
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
                if let Some(message) = lost {
                    div { class: "border-b border-slate-200 dark:border-slate-800 px-3 py-2",
                        Banner { kind: BannerKind::Error, message }
                    }
                }
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
                    // always describe the same table.
                    super::state::Pane::Schema => rsx! {
                        // Above the pane body rather than inside it:
                        // a drop deselects the table it dropped, so a line
                        // living in the body would take the report of the most
                        // consequential operation down with the object it
                        // reported on.
                        SchemaEditLine { id }
                        match selected {
                            Some(table) => rsx! {
                                SchemaPane { key: "schema-{table.key()}", id, table: table.clone() }
                            },
                            None => rsx! {
                                div { class: "flex flex-1 items-center justify-center",
                                    p { class: "text-slate-500", "Select a table to view its schema." }
                                }
                            },
                        }
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
            onkeydown: a_held_key_is_not_a_press,
            onclick: move |_| state.set_pane(id, target, Gesture::CLICK),
            "{label}"
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_shortcut_takes_ctrl_or_cmd() {
        assert!(accel_held(Modifiers::CONTROL));
        assert!(accel_held(Modifiers::META));
        assert!(accel_held(Modifiers::CONTROL | Modifiers::SHIFT));
        assert!(!accel_held(Modifiers::SHIFT));
        assert!(!accel_held(Modifiers::ALT));
        assert!(!accel_held(Modifiers::empty()));
    }

    /// A held Ctrl+E reaches the navigation guard as a repeat, on the whole
    /// wire: the listener sends `pane-repeat` for `e.repeat`, and that
    /// message becomes a repeat gesture (which never confirms the guard —
    /// `nav_guard_confirm_needs_a_matching_intent_and_the_time_floor`).
    #[test]
    fn a_held_pane_shortcut_arrives_as_a_repeat() {
        let code = crate::ui::test_dom::code_lines(GLOBAL_KEYS_JS);
        assert!(
            code.iter()
                .any(|line| line.contains("dioxus.send(e.repeat ? 'pane-repeat' : 'pane')")),
            "the listener no longer says a Ctrl+E is held"
        );
        assert!(pane_shortcut("pane-repeat").is_repeat());
        assert!(!pane_shortcut("pane").is_repeat());
    }

    /// The window listener stands aside while a modal overlay is open, for
    /// every key but `?`: its first statement is the gate, and the gate
    /// reads the attribute every overlay's backdrop carries. Before, `/` and
    /// Ctrl+F moved focus behind an open overlay, after which Escape no
    /// longer reached it, and Ctrl+E/D/B changed the view under it.
    #[test]
    fn the_window_listener_stands_aside_under_a_modal() {
        let code: Vec<String> = crate::ui::test_dom::code_lines(GLOBAL_KEYS_JS);
        let gate = format!(
            "const underModal = (e) => e.key !== '?' && !!document.querySelector('[{}]');",
            crate::ui::js::MODAL_ATTR
        );
        assert_eq!(
            code.iter().filter(|line| **line == gate).count(),
            1,
            "the gate, as written: {code:#?}"
        );
        let listener = code
            .iter()
            .position(|line| line == "window.addEventListener('keydown', (e) => {")
            .expect("the keydown listener is gone");
        assert_eq!(
            code[listener + 1],
            "if (underModal(e)) return;",
            "the gate is not the listener's first statement"
        );
    }

    /// The quit confirmation renders after the cheatsheet, so it is drawn
    /// over it (both overlays share a z-level). The cheatsheet also refuses
    /// to open while the confirmation is up; this is the second guard.
    #[test]
    fn the_quit_confirmation_is_drawn_over_the_cheatsheet() {
        let source = include_str!("shell.rs");
        let lines = crate::ui::test_dom::code_lines(&crate::source_scan::shipped(source));
        let shell = lines
            .iter()
            .position(|line| line.starts_with("pub fn Shell("))
            .expect("Shell is gone");
        let at = |call: &str| {
            lines[shell..]
                .iter()
                .position(|line| line == call)
                .unwrap_or_else(|| panic!("Shell no longer renders `{call}`"))
        };
        assert!(at("Cheatsheet {}") < at("QuitConfirmDialog {}"));
    }

    /// The window listener has no Escape action: every Escape belongs to the
    /// `Overlay` that holds focus. A global one sees the same key as the
    /// overlay's own handler (`stop_propagation` does not reach the real
    /// event), so one Escape would close two things — the cheatsheet and the
    /// form under it, say.
    #[test]
    fn the_window_listener_leaves_escape_to_the_overlays() {
        for line in GLOBAL_KEYS_JS
            .lines()
            .filter(|line| !line.trim_start().starts_with("//"))
        {
            assert!(!line.contains("Escape"), "a window Escape action: {line}");
        }
    }

    /// The window-level shortcuts follow the same rule as [`accel_held`]:
    /// every one reads its modifier through `accel`, and `accel` takes Ctrl
    /// or Cmd. Before, they read `e.ctrlKey` alone, so on macOS Cmd+C copied
    /// in the grid while Cmd+D did nothing.
    #[test]
    fn every_window_shortcut_takes_ctrl_or_cmd() {
        // Code only — a comment naming `e.ctrlKey` must not satisfy, or
        // trip, anything here — with whitespace collapsed so a reflow is not
        // a failure.
        let code: Vec<String> = GLOBAL_KEYS_JS
            .lines()
            .map(|line| line.split_whitespace().collect::<Vec<_>>().join(" "))
            .filter(|line| !line.starts_with("//"))
            .collect();
        let rule = "const accel = (e) => e.ctrlKey || e.metaKey;";
        assert_eq!(
            code.iter().filter(|line| *line == rule).count(),
            1,
            "the rule, as written"
        );
        for line in code.iter().filter(|line| *line != rule) {
            assert!(
                !line.contains("ctrlKey") && !line.contains("metaKey"),
                "a shortcut reads a modifier directly instead of through `accel`: {line}"
            );
        }
        for key in ['f', 'e', 'd', 'b'] {
            let gate = format!("if (accel(e) && (e.key === '{key}' ||");
            assert!(
                code.iter().any(|line| line.contains(&gate)),
                "Ctrl+{key} is not gated on `accel`"
            );
        }
    }
}
